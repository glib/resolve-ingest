use crate::model::{Cancel, Events, progress};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::File,
    os::fd::{AsRawFd, RawFd},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    process::Command,
};

use crate::settings::Verify;

const FULL_PROFILE: &str = "primary-motion-video-copy/all-audio-pcm_s24le/mov/full-decode/v1";
const FAST_PROFILE: &str =
    "primary-motion-video-copy/all-audio-pcm_s24le/mov/structure-checksum/v1";

pub fn profile(verify: Verify) -> &'static str {
    match verify {
        Verify::Fast => FAST_PROFILE,
        Verify::Full => FULL_PROFILE,
    }
}
/// The verification level a recorded batch was planned with, if this version supports it.
pub fn verify_for_profile(profile: &str) -> Option<Verify> {
    [Verify::Fast, Verify::Full]
        .into_iter()
        .find(|v| self::profile(*v) == profile)
}

/// Locate the media tools shipped with an installation before consulting PATH.
/// `RESOLVE_INGEST_FFMPEG_DIR` is useful for portable installs and development.
pub fn tool_path(tool: &str) -> PathBuf {
    let names = [tool.to_string(), format!("{tool}.bin")];
    let mut dirs = Vec::new();
    if let Some(dir) = std::env::var_os("RESOLVE_INGEST_FFMPEG_DIR") {
        dirs.push(PathBuf::from(dir));
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        dirs.push(parent.join("../libexec/resolve-ingest"));
        dirs.push(parent.join("ffmpeg"));
    }
    dirs.push(PathBuf::from("/usr/libexec/resolve-ingest"));
    for dir in dirs {
        for name in &names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from(tool)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_deadline_reaps_child_without_canceling_later_work() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let cancel = Cancel::default();
        let start = std::time::Instant::now();
        let result = process_with_timeout(
            "sh",
            &[
                "-c".into(),
                "echo $$ > \"$1\"; exec sleep 30".into(),
                "probe-test".into(),
                pid_file.to_str().unwrap().into(),
            ],
            &[],
            None,
            &cancel,
            None,
            Some(Duration::from_millis(200)),
        )
        .await;
        assert!(result.err().unwrap().to_string().contains("timed out"));
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "Timed-out child was not reaped"
        );
        assert!(!cancel.requested());
        process("true", &[], &[], None, &cancel, None)
            .await
            .unwrap();
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stream {
    pub index: u32,
    pub codec_type: String,
    #[serde(default = "unknown_codec")]
    pub codec_name: String,
    pub width: Option<u64>,
    pub height: Option<u64>,
    pub pix_fmt: Option<String>,
    pub sample_rate: Option<String>,
    pub channels: Option<u64>,
    pub channel_layout: Option<String>,
    pub avg_frame_rate: Option<String>,
    pub duration: Option<String>,
    pub start_time: Option<String>,
    #[serde(default)]
    pub disposition: Value,
    #[serde(default)]
    pub tags: Value,
    #[serde(default)]
    pub side_data_list: Value,
    pub color_range: Option<String>,
    pub color_space: Option<String>,
    pub color_transfer: Option<String>,
    pub color_primaries: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Value,
}
fn unknown_codec() -> String {
    "unknown".into()
}
pub fn number(s: Option<&str>) -> Option<f64> {
    s.and_then(|s| s.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}
pub fn rate(s: &str) -> Option<f64> {
    let (a, b) = s.split_once('/')?;
    let b = number(Some(b))?;
    (b > 0.).then(|| number(Some(a)).map(|a| a / b)).flatten()
}
impl Probe {
    pub fn video(&self) -> Option<&Stream> {
        self.streams.iter().find(|s| {
            s.codec_type == "video" && s.disposition["attached_pic"].as_u64().unwrap_or(0) == 0
        })
    }
    pub fn audio(&self) -> impl Iterator<Item = &Stream> {
        self.streams.iter().filter(|s| s.codec_type == "audio")
    }
    pub fn duration(&self) -> f64 {
        number(self.format["duration"].as_str()).unwrap_or(0.)
    }
    pub fn estimate(&self, source_size: u64) -> u64 {
        let pcm: f64 = self
            .audio()
            .map(|s| {
                self.duration()
                    * number(s.sample_rate.as_deref()).unwrap_or(192000.)
                    * s.channels.unwrap_or(8) as f64
                    * 3.
            })
            .sum();
        ((source_size as f64 + pcm) * 1.15 + 1024. * 1024.)
            .ceil()
            .min(u64::MAX as f64) as u64
    }
    pub fn notices(&self) -> Vec<String> {
        let mut n = Vec::new();
        if self.audio().count() == 0 {
            n.push("No audio track: video is remuxed without adding audio.".into());
        }
        for s in &self.streams {
            if s.codec_type != "audio" && Some(s.index) != self.video().map(|v| v.index) {
                n.push(format!("Stream {} ({}/{}) is omitted from the editing copy; preserved in the original.", s.index, s.codec_type, s.codec_name));
            }
        }
        n
    }
}

pub struct ProcessOutput {
    pub stdout: Vec<u8>,
    pub stderr: String,
}
/// Child inherits only explicit media descriptors and the process lock. PDEATHSIG handles parent death;
/// inherited flock prevents recovery racing a child that is still exiting (including blocked I/O).
pub async fn process(
    tool: &str,
    args: &[String],
    files: &[&File],
    lock: Option<RawFd>,
    cancel: &Cancel,
    progress_target: Option<(&Events, usize, &str, f64)>,
) -> Result<ProcessOutput> {
    process_with_timeout(tool, args, files, lock, cancel, progress_target, None).await
}

async fn process_with_timeout(
    tool: &str,
    args: &[String],
    files: &[&File],
    lock: Option<RawFd>,
    cancel: &Cancel,
    progress_target: Option<(&Events, usize, &str, f64)>,
    timeout: Option<Duration>,
) -> Result<ProcessOutput> {
    cancel.check()?;
    let executable = tool_path(tool);
    let mut command = Command::new(&executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut fds: Vec<_> = files.iter().map(|f| f.as_raw_fd()).collect();
    if let Some(lock) = lock {
        fds.push(lock);
    }
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe Linux syscalls are called between fork and exec.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(std::io::Error::from_raw_os_error(libc::ECANCELED));
            }
            for fd in &fds {
                if libc::fcntl(*fd, libc::F_SETFD, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().with_context(|| {
        format!(
            "Cannot start {}. Install FFmpeg and ffprobe, then retry.",
            executable.display()
        )
    })?;
    let stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let err = tokio::spawn(async move {
        let mut tail = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = stderr.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            tail.extend_from_slice(&buf[..n]);
            if tail.len() > 65536 {
                tail.drain(..tail.len() - 65536);
            }
        }
        Ok::<_, std::io::Error>(String::from_utf8_lossy(&tail).into_owned())
    });
    let target = progress_target.map(|(e, i, s, d)| (e.clone(), i, s.to_string(), d));
    let out = tokio::spawn(async move {
        let mut reader = BufReader::new(stdout).lines();
        let mut captured = Vec::new();
        while let Some(line) = reader.next_line().await? {
            if let Some((events, i, stage, duration)) = &target
                && let Some(t) = line
                    .strip_prefix("out_time_us=")
                    .and_then(|s| s.parse::<u64>().ok())
            {
                progress(events, *i, stage, t, (duration * 1_000_000.) as u64);
            }
            if captured.len() < 8 * 1024 * 1024 {
                captured.extend_from_slice(line.as_bytes());
                captured.push(b'\n');
            }
        }
        Ok::<_, std::io::Error>(captured)
    });
    let began = std::time::Instant::now();
    let status = loop {
        tokio::select! {
            status = child.wait() => break status?,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let timed_out = timeout.is_some_and(|limit| began.elapsed() >= limit);
                if cancel.requested() || timed_out {
                    if let Some(pid) = child.id() {
                        // SAFETY: this is our live supervised child; termination is followed by wait.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM); }
                    }
                    if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
                        child.kill().await?;
                        child.wait().await?;
                    }
                    let _ = out.await;
                    let _ = err.await;
                    cancel.check()?;
                    anyhow::bail!("{tool} timed out after {} seconds. Check file access and retry its probe.", timeout.unwrap().as_secs_f64());
                }
            }
        }
    };
    let stdout = out.await??;
    let stderr = err.await??;
    ensure!(
        status.success(),
        "{tool} failed ({status}). Check media/tool support and retry.\n{stderr}"
    );
    Ok(ProcessOutput { stdout, stderr })
}
pub fn fd_path(file: &File) -> String {
    format!("/proc/self/fd/{}", file.as_raw_fd())
}
fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|s| (*s).into()).collect()
}
pub async fn probe(file: &File, lock: Option<RawFd>, cancel: &Cancel) -> Result<Probe> {
    let mut args = strings(&[
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ]);
    args.push(fd_path(file));
    let out = process_with_timeout(
        "ffprobe",
        &args,
        &[file],
        lock,
        cancel,
        None,
        Some(Duration::from_secs(30)),
    )
    .await?;
    let probe: Probe = serde_json::from_slice(&out.stdout).context("Invalid ffprobe response")?;
    ensure!(!probe.streams.is_empty(), "No media streams found");
    Ok(probe)
}
pub async fn check_tools(cancel: &Cancel) -> Result<String> {
    let version = process("ffmpeg", &strings(&["-version"]), &[], None, cancel, None).await?;
    process("ffprobe", &strings(&["-version"]), &[], None, cancel, None).await?;
    let enc = process(
        "ffmpeg",
        &strings(&["-hide_banner", "-encoders"]),
        &[],
        None,
        cancel,
        None,
    )
    .await?;
    ensure!(
        String::from_utf8_lossy(&enc.stdout).contains("pcm_s24le"),
        "FFmpeg lacks the pcm_s24le encoder"
    );
    let mux = process(
        "ffmpeg",
        &strings(&["-hide_banner", "-muxers"]),
        &[],
        None,
        cancel,
        None,
    )
    .await?;
    ensure!(
        String::from_utf8_lossy(&mux.stdout)
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some("mov")),
        "FFmpeg lacks the MOV muxer"
    );
    Ok(String::from_utf8_lossy(&version.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .into())
}
pub async fn convert(
    input: &File,
    output: &File,
    expected: &Probe,
    lock: RawFd,
    cancel: &Cancel,
    events: &Events,
    index: usize,
) -> Result<String> {
    let video = expected
        .video()
        .context("No motion video to convert. Select Archive only.")?;
    // -y is safe only here: output is an exclusively created, owned descriptor, never a user pathname.
    let mut args = strings(&[
        "-nostdin",
        "-hide_banner",
        "-y",
        "-xerror",
        "-err_detect",
        "explode",
        "-noautorotate",
        "-i",
    ]);
    args.push(fd_path(input));
    args.extend(strings(&["-map", &format!("0:{}", video.index)]));
    for s in expected.audio() {
        args.extend(strings(&["-map", &format!("0:{}", s.index)]));
    }
    args.extend(strings(&[
        "-map_metadata",
        "0",
        "-map_chapters",
        "0",
        "-c:v",
        "copy",
        "-c:a",
        "pcm_s24le",
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "mov",
    ]));
    args.push(fd_path(output));
    let result = process(
        "ffmpeg",
        &args,
        &[input, output],
        Some(lock),
        cancel,
        Some((events, index, "Converting audio", expected.duration())),
    )
    .await?;
    ensure!(
        String::from_utf8_lossy(&result.stdout).contains("progress=end"),
        "FFmpeg did not finish its progress stream"
    );
    Ok(result.stderr)
}
pub async fn decode(
    file: &File,
    probe: &Probe,
    lock: RawFd,
    cancel: &Cancel,
    events: &Events,
    index: usize,
) -> Result<String> {
    let mut args = strings(&[
        "-nostdin",
        "-v",
        "error",
        "-xerror",
        "-err_detect",
        "explode",
        "-noautorotate",
        "-i",
    ]);
    args.push(fd_path(file));
    for s in &probe.streams {
        if s.codec_type == "audio" || s.codec_type == "video" {
            args.extend(strings(&["-map", &format!("0:{}", s.index)]));
        }
    }
    args.extend(strings(&[
        "-progress",
        "pipe:1",
        "-nostats",
        "-f",
        "null",
        "-",
    ]));
    let output = process(
        "ffmpeg",
        &args,
        &[file],
        Some(lock),
        cancel,
        Some((
            events,
            index,
            "Checking editing copy · full decode",
            probe.duration(),
        )),
    )
    .await?;
    ensure!(
        String::from_utf8_lossy(&output.stdout).contains("progress=end"),
        "Decode did not finish"
    );
    ensure!(
        output.stderr.trim().is_empty(),
        "Decode reported errors: {}",
        output.stderr
    );
    Ok(output.stderr)
}
