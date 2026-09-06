use resolve_ingest::{jobs::Jobs, media, model::Cancel};
use std::{
    fs,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[tokio::test]
async fn cancellation_waits_for_process_exit() {
    let cancel = Cancel::default();
    let signal = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        signal.request();
    });
    let start = Instant::now();
    let result = media::process("sleep", &["30".into()], &[], None, &cancel, None).await;
    assert!(result.is_err());
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
#[ignore = "Helper launched by parent_death_stops_child_and_releases_lock"]
fn supervised_child_helper() {
    let dir = std::env::var_os("RI_SUPERVISION_FIXTURE").expect("fixture path");
    let jobs = Jobs::open(std::path::Path::new(&dir)).unwrap();
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        media::process(
            "sleep",
            &["30".into()],
            &[],
            Some(jobs.lock_fd()),
            &Cancel::default(),
            None,
        )
        .await
        .unwrap();
    });
}

#[test]
fn parent_death_stops_child_and_releases_lock() {
    let dir = tempfile::tempdir().unwrap();
    let mut parent = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "supervised_child_helper",
            "--nocapture",
        ])
        .env("RI_SUPERVISION_FIXTURE", dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let child = loop {
        let mut found = None;
        for task in fs::read_dir(format!("/proc/{}/task", parent.id())).unwrap() {
            let path = task.unwrap().path().join("children");
            if let Ok(text) = fs::read_to_string(path) {
                found = text
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u32>().ok())
                    .or(found);
            }
        }
        if let Some(pid) = found {
            break pid;
        }
        if Instant::now() > deadline {
            let _ = parent.kill();
            panic!("Supervised worker never started");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(Jobs::open(dir.path()).is_err());
    parent.kill().unwrap();
    parent.wait().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(jobs) = Jobs::open(dir.path()) {
            drop(jobs);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "Child retained lock after parent death: {child}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let stat = fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
    assert!(
        stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z"),
        "Child still running after parent died"
    );
}
