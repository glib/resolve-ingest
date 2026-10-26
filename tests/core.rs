use resolve_ingest::{
    jobs::Jobs,
    model::*,
    settings::{Destination, Settings},
    storage::{self, Root},
    verification,
};
use std::{fs, io::Write, path::Path};

fn events() -> Events {
    let (tx, rx) = std::sync::mpsc::sync_channel(64);
    std::thread::spawn(move || while rx.recv().is_ok() {});
    tx
}
#[test]
fn portable_names_stay_readable_and_reject_unusable_names() {
    use std::ffi::OsStr;
    let name = |n: &str| storage::portable_name(OsStr::new(n));
    assert_eq!(
        name("DJI_20260826184226_0027_D.MP4").unwrap(),
        "DJI_20260826184226_0027_D.MP4"
    );
    assert_eq!(name("é:?.mp4").unwrap(), "é__.mp4");
    assert_eq!(name("trailing. ").unwrap(), "trailing");
    assert!(name("..").is_err());
    assert!(name(&"A".repeat(300)).is_err());
}
#[test]
fn mount_parser_decodes_paths_without_retaining_credentials() {
    let mounts = storage::parse_mounts(
        "23 1 0:50 / /mnt/My\\040NAS rw - cifs //nas/video rw,username=secret,password=hidden\n",
    );
    assert_eq!(mounts[0].point, Path::new("/mnt/My NAS"));
    assert_eq!(mounts[0].stable(), "cifs|//nas/video|/");
    assert_eq!(mounts[0].durable_id(), mounts[0].stable());
    // A removable drive is known by its udisks mount point, not its /dev/sdX name.
    let usb = |dev: &str| {
        storage::parse_mounts(&format!(
            "40 1 8:17 / /run/media/me/FOOTAGE rw - exfat {dev} rw\n"
        ))
        .remove(0)
    };
    assert_eq!(usb("/dev/sdb1").durable_id(), usb("/dev/sdc1").durable_id());
    assert!(usb("/dev/sdc1").matches(&usb("/dev/sdb1").durable_id()));
}
#[test]
fn no_clobber_and_symlink_escape_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = Root::source(dir.path()).unwrap();
    assert!(root.capabilities().unwrap().contains("no-clobber"));
    root.create(Path::new(".ri-test.partial"))
        .unwrap()
        .write_all(b"new")
        .unwrap();
    fs::write(dir.path().join("final"), b"existing").unwrap();
    assert!(
        root.publish(Path::new(".ri-test.partial"), Path::new("final"))
            .is_err()
    );
    assert_eq!(fs::read(dir.path().join("final")).unwrap(), b"existing");
    std::os::unix::fs::symlink(outside.path(), dir.path().join("escape")).unwrap();
    assert!(root.create(Path::new("escape/file")).is_err());
    assert!(root.create(Path::new("../file")).is_err());
    assert!(root.remove_owned(Path::new("final")).is_err());
}
#[test]
fn replaced_root_and_wrong_expected_share_fail_closed() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("root");
    fs::create_dir(&path).unwrap();
    let root = Root::source(&path).unwrap();
    fs::rename(&path, temp.path().join("old")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(root.create(Path::new("fallback")).is_err());
    assert!(!path.join("fallback").exists());
    let d = Destination {
        path,
        shared: true,
        expected_mount: "cifs|//missing/share|/".into(),
    };
    assert!(Root::destination(&d).is_err());
}
#[test]
fn overlap_including_symlink_aliases_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("nested")).unwrap();
    assert!(storage::reject_overlap(&[dir.path(), &dir.path().join("nested")]).is_err());
    std::os::unix::fs::symlink(dir.path().join("nested"), dir.path().join("alias")).unwrap();
    assert!(
        storage::reject_overlap(&[&dir.path().join("alias"), &dir.path().join("nested")]).is_err()
    );
}
#[test]
fn hashes_detect_mutation_and_cancel() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("file");
    fs::write(&path, b"original").unwrap();
    let tx = events();
    let expected = verification::hash(
        fs::File::open(&path).unwrap(),
        &Cancel::default(),
        &tx,
        0,
        "hash",
    )
    .unwrap();
    fs::write(&path, b"modified").unwrap();
    let actual = verification::hash(
        fs::File::open(&path).unwrap(),
        &Cancel::default(),
        &tx,
        0,
        "hash",
    )
    .unwrap();
    assert!(verification::compare_hash(&actual, &expected).is_err());
    let cancel = Cancel::default();
    cancel.request();
    assert!(verification::hash(fs::File::open(path).unwrap(), &cancel, &tx, 0, "hash").is_err());
}
#[test]
fn journal_lock_excludes_a_second_writer() {
    let dir = tempfile::tempdir().unwrap();
    let first = Jobs::open(dir.path()).unwrap();
    assert!(Jobs::open(dir.path()).is_err());
    drop(first);
    assert!(Jobs::open(dir.path()).is_ok());
}
#[test]
fn archive_only_is_not_an_editing_success() {
    let item = Item {
        source: Candidate {
            relative: "sidecar.srt".into(),
            identity: Identity {
                dev: 0,
                ino: 0,
                size: 0,
                mtime: 0,
                mtime_ns: 0,
                ctime: 0,
                ctime_ns: 0,
            },
            probe: None,
            probe_error: None,
            selected: true,
            editing: false,
            sidecar_for: None,
            ..Default::default()
        },
        archive_path: "sidecar.srt".into(),
        editing_path: None,
        archive: Stage::Verified,
        editing: Stage::NotRequested,
        archive_evidence: None,
        input_evidence: None,
        output_evidence: None,
        attempts: vec![],
        error: None,
        notices: vec![],
    };
    assert!(item.complete());
    let mut failed = item.clone();
    failed.archive = Stage::Failed;
    assert!(!failed.complete());
    let mut clip = item;
    clip.editing = Stage::Pending;
    assert!(!clip.complete());
}

pub struct Fixture {
    pub dir: tempfile::TempDir,
    pub settings: Settings,
}
impl Default for Fixture {
    fn default() -> Self {
        Self::new()
    }
}

impl Fixture {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for p in ["source", "archive", "editing", "jobs"] {
            fs::create_dir(dir.path().join(p)).unwrap();
        }
        let settings = Settings {
            source: dir.path().join("source"),
            archive: Some(Destination::select(dir.path().join("archive")).unwrap()),
            editing: Some(Destination::select(dir.path().join("editing")).unwrap()),
            ..Default::default()
        };
        Self { dir, settings }
    }
    pub fn candidate(&self, name: &str) -> Candidate {
        Candidate {
            relative: name.into(),
            identity: Identity::of(&fs::metadata(self.settings.source.join(name)).unwrap()),
            probe: None,
            probe_error: Some("Unprobeable original".into()),
            selected: true,
            editing: false,
            sidecar_for: None,
            ..Default::default()
        }
    }
}
#[tokio::test]
async fn review_identifies_the_unwritable_destination_without_copying_media() {
    use std::os::unix::fs::PermissionsExt;

    // Root bypasses ordinary Unix mode checks; run this case as an unprivileged user.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    for (folder, label) in [
        ("archive", "Original archive"),
        ("editing", "Editing destination"),
    ] {
        let f = Fixture::new();
        fs::write(f.settings.source.join("clip.bad"), b"camera bytes").unwrap();
        let mut candidate = f.candidate("clip.bad");
        candidate.editing = folder == "editing";
        let destination = f.dir.path().join(folder);
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o555)).unwrap();
        let result =
            resolve_ingest::pipeline::plan(f.settings.clone(), vec![candidate], &Cancel::default())
                .await;
        fs::set_permissions(&destination, fs::Permissions::from_mode(0o755)).unwrap();
        let error = result.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains(label), "{message}");
        assert!(message.contains(destination.to_str().unwrap()), "{message}");
        assert!(message.contains("writable folder"), "{message}");
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            fs::read(f.settings.source.join("clip.bad")).unwrap(),
            b"camera bytes"
        );
        for name in ["archive", "editing"] {
            assert_eq!(fs::read_dir(f.dir.path().join(name)).unwrap().count(), 0);
        }
    }
}

#[tokio::test]
async fn archive_only_recovery_does_not_need_card_and_detects_corruption() {
    let f = Fixture::new();
    fs::write(f.settings.source.join("clip.bad"), b"camera bytes").unwrap();
    let mut b = resolve_ingest::pipeline::plan(
        f.settings.clone(),
        vec![f.candidate("clip.bad")],
        &Cancel::default(),
    )
    .await
    .unwrap();
    let mut jobs = Jobs::open(&f.dir.path().join("jobs")).unwrap();
    let tx = events();
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert!(b.items[0].complete());
    let archived = f
        .settings
        .archive
        .as_ref()
        .unwrap()
        .path
        .join(&b.items[0].archive_path);
    assert_eq!(fs::read(&archived).unwrap(), b"camera bytes");
    fs::remove_dir_all(&f.settings.source).unwrap();
    // Simulate a crash after publication, before the journal's final state transition.
    b.items[0].archive = Stage::Checking;
    jobs.save(&b).unwrap();
    let mut recovered = jobs.load(&b.id).unwrap();
    resolve_ingest::pipeline::run(&mut jobs, &mut recovered, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert!(recovered.items[0].complete());
    fs::write(&archived, b"changed bytes").unwrap();
    resolve_ingest::pipeline::run(&mut jobs, &mut recovered, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(recovered.items[0].archive, Stage::Failed);
    assert!(!recovered.items[0].complete());
}
#[tokio::test]
async fn changed_source_and_existing_output_never_get_overwritten() {
    let f = Fixture::new();
    fs::write(f.settings.source.join("clip.bad"), b"original").unwrap();
    let mut b = resolve_ingest::pipeline::plan(
        f.settings.clone(),
        vec![f.candidate("clip.bad")],
        &Cancel::default(),
    )
    .await
    .unwrap();
    fs::write(f.settings.source.join("clip.bad"), b"changed longer").unwrap();
    let mut jobs = Jobs::open(&f.dir.path().join("jobs")).unwrap();
    let tx = events();
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(b.items[0].archive, Stage::Failed);
    let target = f
        .settings
        .archive
        .as_ref()
        .unwrap()
        .path
        .join(&b.items[0].archive_path);
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, b"unrelated").unwrap();
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert_eq!(fs::read(target).unwrap(), b"unrelated");
}
#[tokio::test]
async fn cancellation_and_cleanup_preserve_verified_results() {
    let f = Fixture::new();
    fs::write(f.settings.source.join("clip.bad"), b"original").unwrap();
    let mut b = resolve_ingest::pipeline::plan(
        f.settings.clone(),
        vec![f.candidate("clip.bad")],
        &Cancel::default(),
    )
    .await
    .unwrap();
    let mut jobs = Jobs::open(&f.dir.path().join("jobs")).unwrap();
    let tx = events();
    let cancel = Cancel::default();
    cancel.request();
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &cancel, &tx)
        .await
        .unwrap();
    assert_eq!(b.items[0].archive, Stage::Canceled);
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &Cancel::default(), &tx)
        .await
        .unwrap();
    assert!(b.items[0].complete());
    let root = Root::destination(f.settings.archive.as_ref().unwrap()).unwrap();
    let tmp = Path::new(".ri-abandoned.partial");
    root.create(tmp).unwrap().write_all(b"partial").unwrap();
    b.items[0].attempts.push(Attempt {
        id: "abandoned".into(),
        started_at: now(),
        archive_partial: Some(tmp.into()),
        editing_partial: None,
        staging_partial: None,
        error: None,
    });
    assert_eq!(resolve_ingest::pipeline::cleanup(&jobs, &b).unwrap(), 7);
    assert!(root.exists(&b.items[0].archive_path).unwrap());
}

#[tokio::test]
async fn journal_migrates_v1_and_updates_one_item_without_losing_neighbors() {
    let f = Fixture::new();
    fs::write(f.settings.source.join("clip.bad"), b"original").unwrap();
    let mut batch = resolve_ingest::pipeline::plan(
        f.settings.clone(),
        vec![f.candidate("clip.bad")],
        &Cancel::default(),
    )
    .await
    .unwrap();
    batch.items = (0..1000)
        .map(|n| {
            let mut item = batch.items[0].clone();
            item.archive_path = format!("originals/{n}").into();
            item
        })
        .collect();
    let job_dir = f.dir.path().join("jobs");
    let db = rusqlite::Connection::open(job_dir.join("jobs.sqlite3")).unwrap();
    db.execute_batch("CREATE TABLE batches(id TEXT PRIMARY KEY, created_at TEXT NOT NULL, status TEXT NOT NULL, manifest TEXT NOT NULL); PRAGMA user_version=1;").unwrap();
    db.execute(
        "INSERT INTO batches VALUES(?1,?2,?3,?4)",
        rusqlite::params![
            batch.id,
            batch.created_at,
            batch.status,
            serde_json::to_string(&batch).unwrap()
        ],
    )
    .unwrap();
    drop(db);
    let mut jobs = Jobs::open(&job_dir).unwrap();
    assert_eq!(jobs.load(&batch.id).unwrap().items.len(), 1000);
    batch.items[500].archive = Stage::Copying;
    jobs.save_item(&batch, 500).unwrap();
    let recovered = jobs.load(&batch.id).unwrap();
    assert_eq!(recovered.items[500].archive, Stage::Copying);
    assert_eq!(recovered.items[499].archive, Stage::Pending);
    drop(jobs);
    let db = rusqlite::Connection::open(job_dir.join("jobs.sqlite3")).unwrap();
    let journal_bytes: i64 = db
        .query_row("SELECT SUM(length(manifest)) FROM transitions", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(
        journal_bytes < 10_000,
        "Per-item transition unexpectedly included the full 1,000-item batch"
    );
}

#[test]
fn camera_data_streams_can_omit_codec_name() {
    let probe: resolve_ingest::media::Probe = serde_json::from_value(
        serde_json::json!({"streams":[{"index":2,"codec_type":"data"}],"format":{}}),
    )
    .unwrap();
    assert_eq!(probe.streams[0].codec_name, "unknown");
    assert_eq!(probe.notices().len(), 2);
}

#[tokio::test]
async fn archive_layout_is_readable_by_day_and_suffixes_clashes_with_their_sidecars() {
    let f = Fixture::new();
    for dir in ["DCIM/A", "DCIM/B"] {
        fs::create_dir_all(f.settings.source.join(dir)).unwrap();
    }
    // One shared recording time: same day folder, and planning order falls back to the path.
    let shot = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
    for name in ["DCIM/A/GX01.MP4", "DCIM/B/gx01.mp4", "DCIM/B/gx01.LRF"] {
        let path = f.settings.source.join(name);
        fs::write(&path, name).unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(shot)
            .unwrap();
    }
    let mut sidecar = f.candidate("DCIM/B/gx01.LRF");
    sidecar.sidecar_for = Some("DCIM/B/gx01.mp4".into());
    let candidates = vec![
        f.candidate("DCIM/A/GX01.MP4"),
        sidecar,
        f.candidate("DCIM/B/gx01.mp4"),
    ];
    let day = resolve_ingest::catalog::recorded_day(&candidates[0]).to_string();
    // An unrelated file already holds the second clip's preferred name.
    fs::create_dir_all(f.dir.path().join("archive").join(&day)).unwrap();
    fs::write(
        f.dir.path().join("archive").join(&day).join("gx01-2.mp4"),
        b"other",
    )
    .unwrap();
    let b = resolve_ingest::pipeline::plan(f.settings.clone(), candidates, &Cancel::default())
        .await
        .unwrap();
    let mut paths: Vec<_> = b
        .items
        .iter()
        .map(|i| i.archive_path.to_string_lossy().into_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            format!("{day}/GX01.MP4"),
            format!("{day}/gx01-3.LRF"),
            format!("{day}/gx01-3.mp4"),
        ]
    );
}

#[tokio::test]
async fn repeat_ingest_skips_clips_already_verified_in_the_archive() {
    let f = Fixture::new();
    fs::write(f.settings.source.join("clip.mp4"), b"camera bytes").unwrap();
    fs::write(f.settings.source.join("new.mp4"), b"new camera bytes").unwrap();
    let cancel = Cancel::default();
    let mut jobs = Jobs::open(&f.dir.path().join("jobs")).unwrap();
    let archive = f.settings.archive.clone().unwrap();
    let mut first = resolve_ingest::discovery::scan(&f.settings.source, &cancel)
        .await
        .unwrap();
    first.retain(|c| c.relative == Path::new("clip.mp4"));
    let mut b = resolve_ingest::pipeline::plan(f.settings.clone(), first, &cancel)
        .await
        .unwrap();
    resolve_ingest::pipeline::run(&mut jobs, &mut b, None, &cancel, &events())
        .await
        .unwrap();
    assert_eq!(b.status, "Completed");

    let mut again = resolve_ingest::discovery::scan(&f.settings.source, &cancel)
        .await
        .unwrap();
    assert_eq!(
        resolve_ingest::pipeline::mark_archived(&jobs, &archive, &mut again).unwrap(),
        1
    );
    let clip = again
        .iter()
        .find(|c| c.relative == Path::new("clip.mp4"))
        .unwrap();
    assert!(!clip.selected);
    assert_eq!(clip.archived.as_ref(), Some(&b.items[0].archive_path));
    assert!(
        again
            .iter()
            .find(|c| c.relative == Path::new("new.mp4"))
            .unwrap()
            .selected
    );

    // A recorded copy that is no longer in the archive does not count.
    fs::remove_file(archive.path.join(&b.items[0].archive_path)).unwrap();
    let mut rescanned = resolve_ingest::discovery::scan(&f.settings.source, &cancel)
        .await
        .unwrap();
    assert_eq!(
        resolve_ingest::pipeline::mark_archived(&jobs, &archive, &mut rescanned).unwrap(),
        0
    );
}

#[test]
fn verification_profiles_round_trip_including_earlier_full_decode_batches() {
    use resolve_ingest::{media, settings::Verify};
    for v in [Verify::Fast, Verify::Full] {
        assert_eq!(media::verify_for_profile(media::profile(v)), Some(v));
    }
    assert_eq!(
        media::verify_for_profile(
            "primary-motion-video-copy/all-audio-pcm_s24le/mov/full-decode/v1"
        ),
        Some(Verify::Full)
    );
    assert_eq!(media::verify_for_profile("unknown"), None);
}
