use super::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    process::Command,
    time::Duration,
};

async fn fixture(count: usize) -> (tempfile::TempDir, Batch, Jobs) {
    let dir = tempfile::tempdir().unwrap();
    for name in ["source", "archive", "editing", "jobs"] {
        fs::create_dir(dir.path().join(name)).unwrap();
    }
    let first = dir.path().join("source/0.mp4");
    let result = Command::new("ffmpeg")
        .args([
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=96x64:rate=30:duration=1",
            "-f",
            "lavfi",
            "-i",
            "sine=sample_rate=48000:duration=1",
            "-c:v",
            "mpeg4",
            "-c:a",
            "aac",
        ])
        .arg(&first)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    for i in 1..count {
        fs::copy(&first, dir.path().join(format!("source/{i}.mp4"))).unwrap();
    }
    let settings = Settings {
        source: dir.path().join("source"),
        archive: Some(Destination::select(dir.path().join("archive")).unwrap()),
        editing: Some(Destination::select(dir.path().join("editing")).unwrap()),
        ..Default::default()
    };
    let candidates = crate::discovery::scan(&settings.source, &Cancel::default())
        .await
        .unwrap();
    let batch = plan(settings, candidates, &Cancel::default())
        .await
        .unwrap();
    let jobs = Jobs::open(&dir.path().join("jobs")).unwrap();
    (dir, batch, jobs)
}

#[tokio::test]
async fn conversion_runs_ahead_of_archive_with_two_assets_and_one_worker_per_lane() {
    let (dir, mut batch, mut jobs) = fixture(3).await;
    let lanes = workers::Lanes::default();
    let archive_gate = lanes.archive.clone().acquire_owned().await.unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let root = dir.path().to_path_buf();
    let observer = std::thread::spawn(move || {
        let mut gate = Some(archive_gate);
        let mut prepared = BTreeSet::new();
        let mut started = BTreeSet::new();
        let mut active = BTreeMap::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(20)) {
            match event {
                Event::Activity {
                    lane,
                    item,
                    active: true,
                } => {
                    assert!(
                        active.insert(lane, item).is_none(),
                        "Two workers in {lane:?}"
                    );
                }
                Event::Activity {
                    lane,
                    item,
                    active: false,
                } => {
                    assert_eq!(active.remove(&lane), Some(item));
                }
                Event::Item { index, item } if gate.is_some() => {
                    started.insert(index);
                    assert!(started.len() <= MAX_IN_FLIGHT, "Unbounded asset queue");
                    if item.editing == Stage::AwaitingArchive
                        && let Some(evidence) = &item.output_evidence
                    {
                        assert_ne!(item.archive, Stage::Verified);
                        let attempt = item.attempts.last().unwrap();
                        assert!(
                            root.join("editing")
                                .join(attempt.staging_partial.as_ref().unwrap())
                                .is_file()
                        );
                        assert!(
                            root.join("editing")
                                .join(attempt.editing_partial.as_ref().unwrap())
                                .is_file()
                        );
                        assert!(
                            !root
                                .join("editing")
                                .join(item.editing_path.as_ref().unwrap())
                                .exists()
                        );
                        // The temporary path and evidence are durable before workers can publish.
                        let db =
                            rusqlite::Connection::open(root.join("jobs/jobs.sqlite3")).unwrap();
                        let saved: String = db
                            .query_row(
                                "SELECT manifest FROM batch_items WHERE position=?1",
                                [index as i64],
                                |r| r.get(0),
                            )
                            .unwrap();
                        let saved: Item = serde_json::from_str(&saved).unwrap();
                        assert_eq!(
                            saved.output_evidence.as_ref().unwrap().sha256,
                            evidence.sha256
                        );
                        prepared.insert(index);
                        if prepared.len() == MAX_IN_FLIGHT {
                            drop(gate.take());
                        }
                    }
                }
                _ => (),
            }
        }
        assert_eq!(
            prepared.len(),
            MAX_IN_FLIGHT,
            "Local conversions waited for the archive lane"
        );
        assert!(active.is_empty(), "Workers still active on return");
    });
    run_with_lanes(&mut jobs, &mut batch, None, &Cancel::default(), &tx, lanes)
        .await
        .unwrap();
    drop(tx);
    observer.join().unwrap();
    assert_eq!(batch.status, "Completed");
    assert_eq!(batch.verified_fraction(), 1.);
    for item in &batch.items {
        assert!(
            item.output_evidence
                .as_ref()
                .unwrap()
                .checks
                .iter()
                .any(|s| s.contains("local conversion input"))
        );
        assert!(
            !dir.path()
                .join("editing")
                .join(
                    item.attempts
                        .last()
                        .unwrap()
                        .staging_partial
                        .as_ref()
                        .unwrap()
                )
                .exists()
        );
        assert_eq!(
            fs::read(dir.path().join("archive").join(&item.archive_path)).unwrap(),
            fs::read(dir.path().join("source").join(&item.source.relative)).unwrap()
        );
    }
}

#[tokio::test]
async fn archive_failure_preserves_original_and_keeps_converted_output_unpublished() {
    let (dir, mut batch, mut jobs) = fixture(1).await;
    let lanes = workers::Lanes::default();
    let gate = lanes.archive.clone().acquire_owned().await.unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let archive = dir.path().join("archive");
    let observer = std::thread::spawn(move || {
        let mut gate = Some(gate);
        let mut saw_prepared = false;
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(20)) {
            if let Event::Item { item, .. } = event
                && gate.is_some()
                && item.editing == Stage::AwaitingArchive
                && item.output_evidence.is_some()
            {
                // A symlink destination fails closed, independently of test-user privileges.
                fs::rename(&archive, archive.with_file_name("archive-disconnected")).unwrap();
                std::os::unix::fs::symlink(
                    archive.with_file_name("archive-disconnected"),
                    &archive,
                )
                .unwrap();
                saw_prepared = true;
                drop(gate.take());
            }
        }
        assert!(saw_prepared);
    });
    run_with_lanes(&mut jobs, &mut batch, None, &Cancel::default(), &tx, lanes)
        .await
        .unwrap();
    drop(tx);
    observer.join().unwrap();
    let item = &batch.items[0];
    assert_eq!(item.archive, Stage::Failed);
    assert_eq!(item.editing, Stage::Failed);
    assert!(
        !dir.path()
            .join("editing")
            .join(item.editing_path.as_ref().unwrap())
            .exists()
    );
    assert!(dir.path().join("source/0.mp4").is_file());
    // Restore access, discard only recorded partials, then recover without overwriting anything.
    fs::remove_file(dir.path().join("archive")).unwrap();
    fs::rename(
        dir.path().join("archive-disconnected"),
        dir.path().join("archive"),
    )
    .unwrap();
    assert!(cleanup(&jobs, &batch).unwrap() > 0);
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let drain = std::thread::spawn(move || while rx.recv().is_ok() {});
    run(&mut jobs, &mut batch, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    drop(tx);
    drain.join().unwrap();
    assert_eq!(batch.status, "Completed");
}

#[tokio::test]
async fn cancellation_drains_all_lanes_and_retains_journaled_partials_for_cleanup() {
    let (dir, mut batch, mut jobs) = fixture(3).await;
    let lanes = workers::Lanes::default();
    let gate = lanes.archive.clone().acquire_owned().await.unwrap();
    let cancel = Cancel::default();
    let signal = cancel.clone();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let observer = std::thread::spawn(move || {
        let mut gate = Some(gate);
        let mut active = BTreeMap::new();
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(20)) {
            match event {
                Event::Activity {
                    lane,
                    item,
                    active: true,
                } => {
                    active.insert(lane, item);
                }
                Event::Activity {
                    lane,
                    active: false,
                    ..
                } => {
                    active.remove(&lane);
                }
                Event::Item { item, .. } if item.editing == Stage::Converting && gate.is_some() => {
                    signal.request();
                    drop(gate.take());
                }
                _ => (),
            }
        }
        assert!(gate.is_none(), "Conversion never started");
        assert!(
            active.is_empty(),
            "Cancellation returned before workers stopped"
        );
    });
    run_with_lanes(&mut jobs, &mut batch, None, &cancel, &tx, lanes)
        .await
        .unwrap();
    drop(tx);
    observer.join().unwrap();
    assert_eq!(batch.status, "Canceled");
    assert!(
        batch.items[2].attempts.is_empty(),
        "Started another asset after cancellation"
    );
    for item in &batch.items {
        assert!(
            !dir.path()
                .join("editing")
                .join(item.editing_path.as_ref().unwrap())
                .exists()
        );
    }
    cleanup(&jobs, &batch).unwrap();
    assert!(stale_partials(&batch).unwrap().is_empty());
}

#[tokio::test]
async fn replaced_local_copy_is_rejected_before_conversion() {
    let (dir, mut batch, mut jobs) = fixture(1).await;
    let lanes = workers::Lanes::default();
    let gate = lanes.editing.clone().acquire_owned().await.unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let editing = dir.path().join("editing");
    let observer = std::thread::spawn(move || {
        let mut gate = Some(gate);
        while let Ok(event) = rx.recv_timeout(Duration::from_secs(20)) {
            if let Event::Item { item, .. } = event
                && gate.is_some()
                && item.editing == Stage::Pending
                && let Some(path) = item
                    .attempts
                    .last()
                    .and_then(|a| a.staging_partial.as_ref())
            {
                let path = editing.join(path);
                if path.exists() {
                    let replacement = editing.join("replacement");
                    fs::write(&replacement, fs::read(&path).unwrap()).unwrap();
                    fs::rename(replacement, path).unwrap();
                    drop(gate.take());
                }
            }
        }
        assert!(gate.is_none());
    });
    run_with_lanes(&mut jobs, &mut batch, None, &Cancel::default(), &tx, lanes)
        .await
        .unwrap();
    drop(tx);
    observer.join().unwrap();
    assert_eq!(batch.items[0].archive, Stage::Verified);
    assert_eq!(batch.items[0].editing, Stage::Failed);
    assert!(
        batch.items[0]
            .error
            .as_ref()
            .unwrap()
            .contains("local copy changed before conversion")
    );
    assert!(
        !dir.path()
            .join("editing")
            .join(batch.items[0].editing_path.as_ref().unwrap())
            .exists()
    );
}

#[tokio::test]
async fn journal_recovers_new_stages_and_accepts_older_attempts() {
    let (dir, mut batch, mut jobs) = fixture(1).await;
    batch.status = "Running".into();
    batch.items[0].archive = Stage::Checking;
    batch.items[0].editing = Stage::AwaitingArchive;
    let old_attempt = serde_json::json!({
        "id": "old", "started_at": now(), "archive_partial": null,
        "editing_partial": null, "error": null
    });
    let old: Attempt = serde_json::from_value(old_attempt).unwrap();
    assert!(old.staging_partial.is_none());
    batch.items[0].attempts.push(old);
    jobs.save(&batch).unwrap();
    drop(jobs);
    let mut jobs = Jobs::open(&dir.path().join("jobs")).unwrap();
    let loaded = jobs.load(&batch.id).unwrap();
    assert_eq!(loaded.items[0].archive, Stage::Interrupted);
    assert_eq!(loaded.items[0].editing, Stage::Interrupted);
    batch.items[0].editing = Stage::Staging;
    jobs.save(&batch).unwrap();
    drop(jobs);
    let jobs = Jobs::open(&dir.path().join("jobs")).unwrap();
    assert_eq!(
        jobs.load(&batch.id).unwrap().items[0].editing,
        Stage::Interrupted
    );
}

#[tokio::test]
async fn conversion_only_writes_beside_sources_without_archiving_or_overwriting() {
    let (dir, original_batch, mut jobs) = fixture(2).await;
    let mut settings = original_batch.settings;
    settings.workflow = Workflow::Convert;
    settings.archive = None;
    settings.editing = Some(Destination::select(settings.source.clone()).unwrap());
    let candidates: Vec<_> = original_batch.items.into_iter().map(|i| i.source).collect();
    let original = fs::read(dir.path().join("source/0.mp4")).unwrap();
    let mut batch = plan(settings.clone(), candidates.clone(), &Cancel::default())
        .await
        .unwrap();
    assert_eq!(batch.staging_bytes(), 0);
    assert_eq!(batch.archive_bytes, 0);
    assert_eq!(
        batch.items[0].editing_path.as_deref(),
        Some(Path::new("0.mp4.mov"))
    );
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    let drain = std::thread::spawn(move || while rx.recv().is_ok() {});
    run(&mut jobs, &mut batch, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(batch.status, "Completed");
    assert_eq!(batch.verified_fraction(), 1.);
    assert_eq!(fs::read(dir.path().join("source/0.mp4")).unwrap(), original);
    assert_eq!(fs::read_dir(dir.path().join("archive")).unwrap().count(), 0);
    for item in &batch.items {
        assert_eq!(item.archive, Stage::NotRequested);
        assert!(item.archive_evidence.is_none());
        assert!(item.input_evidence.is_some());
        assert!(
            item.attempts
                .iter()
                .all(|a| a.archive_partial.is_none() && a.staging_partial.is_none())
        );
        assert!(
            dir.path()
                .join("source")
                .join(item.editing_path.as_ref().unwrap())
                .is_file()
        );
    }
    let converted = fs::read(dir.path().join("source/0.mp4.mov")).unwrap();
    let conflict = plan(settings, candidates, &Cancel::default())
        .await
        .unwrap_err();
    assert!(conflict.to_string().contains("already exists"));
    run(&mut jobs, &mut batch, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(batch.status, "Completed");
    assert_eq!(
        fs::read(dir.path().join("source/0.mp4.mov")).unwrap(),
        converted
    );
    fs::write(dir.path().join("source/0.mp4"), b"changed source").unwrap();
    run(&mut jobs, &mut batch, Some(0), &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(batch.items[0].archive, Stage::NotRequested);
    assert_eq!(batch.items[0].editing, Stage::Failed);
    assert_eq!(
        fs::read(dir.path().join("source/0.mp4.mov")).unwrap(),
        converted
    );
    cleanup(&jobs, &batch).unwrap();
    drop(tx);
    drain.join().unwrap();
}
