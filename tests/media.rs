use resolve_ingest::{
    discovery,
    jobs::Jobs,
    media,
    model::*,
    pipeline,
    settings::{Destination, Settings},
};
use std::{fs, process::Command};

fn generate(path: &std::path::Path, audio: bool, multitrack: bool) {
    let mut c = Command::new("ffmpeg");
    c.args([
        "-v",
        "error",
        "-f",
        "lavfi",
        "-i",
        "testsrc2=size=96x64:rate=30:duration=1",
    ]);
    if audio {
        c.args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=1",
        ]);
    }
    if multitrack {
        c.args([
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=44100:duration=1",
        ]);
    }
    c.args(["-map", "0:v:0"]);
    if audio {
        c.args(["-map", "1:a:0"]);
    }
    if multitrack {
        c.args(["-map", "2:a:0"]);
    }
    let out = c
        .args(["-c:v", "mpeg4", "-c:a", "aac", "-f", "mp4"])
        .arg(path)
        .output()
        .expect("FFmpeg required for integration tests");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
#[tokio::test]
async fn generated_media_ingest_retries_and_content_proof() {
    let d = tempfile::tempdir().unwrap();
    for folder in ["source", "archive", "editing", "jobs"] {
        fs::create_dir(d.path().join(folder)).unwrap();
    }
    let source = d.path().join("source");
    generate(&source.join("clip.mp4"), true, true);
    generate(&source.join("clip.mov"), false, false);
    generate(&source.join("extensionless"), true, false);
    fs::write(source.join("clip.srt"), b"sidecar").unwrap();
    fs::write(source.join("broken.MP4"), b"broken camera original").unwrap();
    fs::write(source.join("notes.txt"), b"ordinary file").unwrap();
    std::os::unix::fs::symlink(source.join("clip.mp4"), source.join("symlink.mp4")).unwrap();
    let cancel = Cancel::default();
    let mut candidates = discovery::scan(&source, &cancel).await.unwrap();
    assert_eq!(candidates.len(), 6);
    assert_eq!(candidates.iter().filter(|c| c.editing).count(), 3);
    assert!(
        candidates
            .iter()
            .find(|c| c.relative == std::path::Path::new("clip.srt"))
            .unwrap()
            .selected
    );
    assert!(
        !candidates
            .iter()
            .find(|c| c.relative == std::path::Path::new("notes.txt"))
            .unwrap()
            .selected
    );
    let settings = Settings {
        source: source.clone(),
        archive: Some(Destination::select(d.path().join("archive"), false).unwrap()),
        editing: Some(Destination::select(d.path().join("editing"), false).unwrap()),
        ..Default::default()
    };
    candidates
        .iter_mut()
        .find(|c| c.relative == std::path::Path::new("extensionless"))
        .unwrap()
        .editing = false;
    let mut batch = pipeline::plan(settings, candidates, &cancel).await.unwrap();
    let mut jobs = Jobs::open(&d.path().join("jobs")).unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    std::thread::spawn(move || while rx.recv().is_ok() {});
    pipeline::run(&mut jobs, &mut batch, None, &cancel, &tx)
        .await
        .unwrap();
    assert_eq!(
        batch.status,
        "Completed",
        "{}",
        serde_json::to_string_pretty(&batch).unwrap()
    );
    assert_eq!(batch.items.len(), 5);
    for item in &batch.items {
        let original = d.path().join("archive").join(&item.archive_path);
        assert_eq!(
            fs::read(&original).unwrap(),
            fs::read(source.join(&item.source.relative)).unwrap()
        );
        if let Some(p) = &item.editing_path {
            let output = d.path().join("editing").join(p);
            let probe = media::probe(
                &fs::File::open(&output).unwrap(),
                Some(jobs.lock_fd()),
                &cancel,
            )
            .await
            .unwrap();
            assert!(probe.audio().all(|s| s.codec_name == "pcm_s24le"));
            // POC evidence: decoded video frame checksums, separate from runtime timing checks.
            let hash = |path: &std::path::Path, map: &str, audio: bool| {
                let mut c = Command::new("ffmpeg");
                c.args(["-v", "error", "-i"]).arg(path).args(["-map", map]);
                if audio {
                    c.args(["-c:a", "pcm_s24le", "-f", "s24le", "-"]);
                } else {
                    c.args(["-f", "framemd5", "-"]);
                }
                let out = c.output().unwrap();
                assert!(out.status.success());
                out.stdout
            };
            let frame_hashes = |bytes: Vec<u8>| {
                String::from_utf8(bytes)
                    .unwrap()
                    .lines()
                    .filter(|l| !l.starts_with('#'))
                    .map(|l| l.rsplit(',').next().unwrap().trim().to_string())
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                frame_hashes(hash(&original, "0:v:0", false)),
                frame_hashes(hash(&output, "0:v:0", false))
            );
            for n in 0..probe.audio().count() {
                assert_eq!(
                    hash(&original, &format!("0:a:{n}"), true),
                    hash(&output, &format!("0:a:{n}"), true)
                );
            }
        }
    }
    fs::remove_dir_all(&source).unwrap();
    let archived_index = batch
        .items
        .iter()
        .position(|i| i.source.relative == std::path::Path::new("extensionless"))
        .unwrap();
    pipeline::prepare_archived_editing(&mut jobs, &mut batch, archived_index, &cancel, &tx)
        .await
        .unwrap();
    assert_eq!(batch.items[archived_index].editing, Stage::Pending);
    pipeline::run(&mut jobs, &mut batch, None, &cancel, &tx)
        .await
        .unwrap();
    assert_eq!(batch.status, "Completed");
    let index = batch
        .items
        .iter()
        .position(|i| i.editing_path.is_some())
        .unwrap();
    fs::write(
        d.path()
            .join("editing")
            .join(batch.items[index].editing_path.as_ref().unwrap()),
        b"corrupted output",
    )
    .unwrap();
    pipeline::run(&mut jobs, &mut batch, Some(index), &cancel, &tx)
        .await
        .unwrap();
    assert_eq!(batch.items[index].archive, Stage::Verified);
    assert_eq!(batch.items[index].editing, Stage::Failed);
}
