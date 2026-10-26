//! `resolve-ingest demo`: the real window driven by synthetic footage. Nothing is read or written
//! except validated folder choices, which are not saved.
use super::worker::{Command, LocationInfo, Reply, Scope, fresh_cancel, next};
use crate::{
    model::{Batch, Cancel, Candidate, Event, Events, Evidence, Identity, Item, Lane, Stage},
    settings::{Destination, Workflow},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

const GIB: u64 = 1024 * 1024 * 1024;

pub(super) fn run(
    rx: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Reply>,
    events: Events,
    current: Arc<Mutex<Cancel>>,
) {
    let _ = tx.send(Reply::Ready { interrupted: 0 });
    let mut queue = VecDeque::new();
    let mut history: Vec<Batch> = vec![];
    while let Some(command) = next(&rx, &mut queue) {
        let cancel = fresh_cancel(&current);
        let reply = match command {
            Command::Scan {
                source, workflow, ..
            } => {
                let clips = footage(workflow);
                for (i, c) in clips.iter().enumerate() {
                    let _ = tx.send(Reply::ScanProgress {
                        count: i,
                        path: c.relative.clone(),
                    });
                    std::thread::sleep(Duration::from_millis(40));
                }
                Reply::Scanned {
                    source,
                    result: Ok(clips),
                }
            }
            Command::Preflight {
                generation,
                settings,
                candidates,
                ..
            } => {
                std::thread::sleep(Duration::from_millis(300));
                Reply::Planned {
                    generation,
                    result: Ok(Box::new(plan(*settings, candidates))),
                }
            }
            Command::Start(mut batch) => {
                let indices: Vec<_> = (0..batch.items.len()).collect();
                simulate(&mut batch, &indices, &cancel, &events, true);
                history.retain(|b| b.id != batch.id);
                history.insert(0, (*batch).clone());
                Reply::Finished { batch, error: None }
            }
            Command::Retry { batch, scope } => match history.iter_mut().find(|b| b.id == batch) {
                Some(batch) => {
                    let indices: Vec<_> = (0..batch.items.len())
                        .filter(|&i| match scope {
                            Scope::Item(n) => i == n,
                            Scope::Incomplete => !batch.items[i].complete(),
                        })
                        .collect();
                    simulate(batch, &indices, &cancel, &events, false);
                    Reply::Finished {
                        batch: Box::new(batch.clone()),
                        error: None,
                    }
                }
                None => Reply::Error("Unknown demo batch".into()),
            },
            Command::ValidateDestination { kind, path } => Reply::Destination {
                kind,
                result: Destination::select(path).map_err(|e| format!("{e:#}")),
            },
            Command::Locations(settings) => {
                let info = |d: &Destination, kind, free| LocationInfo {
                    name: d.name(),
                    kind,
                    free: Some(free),
                    error: None,
                };
                Reply::Locations {
                    archive: settings
                        .archive
                        .as_ref()
                        .map(|d| info(d, "Network share", 2_310 * GIB)),
                    editing: settings
                        .editing
                        .as_ref()
                        .map(|d| info(d, "Local disk", 412 * GIB)),
                }
            }
            Command::History => Reply::History(history.clone()),
            _ => Reply::Info("Not available in the demo".into()),
        };
        if tx.send(reply).is_err() {
            break;
        }
    }
}

fn probe(duration: f64) -> crate::media::Probe {
    serde_json::from_value(serde_json::json!({
        "streams": [
            {"index": 0, "codec_type": "video", "codec_name": "hevc", "width": 3840, "height": 2160,
             "pix_fmt": "yuv420p10le", "avg_frame_rate": "30000/1001"},
            {"index": 1, "codec_type": "audio", "codec_name": "aac", "sample_rate": "48000", "channels": 2}
        ],
        "format": {"duration": format!("{duration}")}
    }))
    .unwrap()
}

fn footage(workflow: Workflow) -> Vec<Candidate> {
    let mut clips = vec![];
    let days = [
        ("20261021", 5, true),
        ("20261023", 7, false),
        ("20261024", 9, false),
    ];
    let mut n = 0;
    for (day, count, archived) in days {
        for i in 0..count {
            n += 1;
            let time = format!("{:02}{:02}{:02}", 9 + i, (i * 7) % 60, (i * 13) % 60);
            let name = format!("DJI_{day}{time}_{n:04}_D");
            let duration = 20. + ((n * 37) % 240) as f64;
            let size = (duration * 12_500_000.) as u64;
            let relative = PathBuf::from("DCIM/DJI_001").join(format!("{name}.MP4"));
            let unreadable = n == 15;
            clips.push(Candidate {
                identity: Identity {
                    size,
                    ino: n as u64,
                    ..Default::default()
                },
                probe: (!unreadable).then(|| probe(duration)),
                probe_error: unreadable
                    .then(|| "ffprobe failed: moov atom not found (file is incomplete)".into()),
                selected: !archived,
                editing: !unreadable,
                archived: archived.then(|| format!("2026-10-21/{name}.MP4").into()),
                fingerprint: Some(name.clone()),
                relative: relative.clone(),
                sidecar_for: None,
            });
            if workflow == Workflow::Ingest && i % 3 == 0 {
                clips.push(Candidate {
                    relative: relative.with_extension("LRF"),
                    identity: Identity {
                        size: size / 40,
                        ino: 1000 + n as u64,
                        ..Default::default()
                    },
                    selected: !archived,
                    sidecar_for: Some(relative),
                    archived: archived.then(|| format!("2026-10-21/{name}.LRF").into()),
                    ..Default::default()
                });
            }
        }
    }
    clips.sort_by(|a, b| a.relative.cmp(&b.relative));
    clips
}

fn plan(settings: crate::settings::Settings, candidates: Vec<Candidate>) -> Batch {
    let mut selected: Vec<_> = candidates.into_iter().filter(|c| c.selected).collect();
    selected.sort_by_cached_key(|c| std::cmp::Reverse(crate::catalog::recorded(c).date));
    let items: Vec<_> = selected
        .into_iter()
        .map(|c| {
            let day = crate::catalog::recorded_day(&c).to_string();
            let name = c
                .relative
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let converting = settings.workflow == Workflow::Convert;
            Item {
                archive_path: if converting {
                    PathBuf::new()
                } else {
                    PathBuf::from(&day).join(&name)
                },
                editing_path: c
                    .editing
                    .then(|| PathBuf::from(&day).join(&name).with_extension("mov")),
                archive: if converting {
                    Stage::NotRequested
                } else {
                    Stage::Pending
                },
                editing: if c.editing {
                    Stage::Pending
                } else {
                    Stage::NotRequested
                },
                source: c,
                archive_evidence: None,
                input_evidence: None,
                output_evidence: None,
                attempts: vec![],
                error: None,
                notices: vec![],
            }
        })
        .collect();
    let archive_bytes = items.iter().map(|i| i.source.identity.size).sum();
    Batch {
        schema: 1,
        id: uuid::Uuid::new_v4().to_string(),
        created_at: crate::model::now(),
        profile: crate::media::profile(settings.verify).into(),
        settings,
        tools: None,
        editing_bytes: archive_bytes * 23 / 20,
        archive_bytes,
        items,
        skipped: 0,
        status: "Ready".into(),
        storage_notes: vec![],
    }
}

fn evidence(bytes: u64) -> Evidence {
    Evidence {
        sha256: format!("{:064x}", bytes.wrapping_mul(0x9e37_79b9_7f4a_7c15)),
        bytes,
        checked_at: crate::model::now(),
        checks: vec!["Synthetic demo evidence".into()],
    }
}

/// Plays plausible stage progress; the first run fails one conversion so recovery can be tried.
fn simulate(batch: &mut Batch, indices: &[usize], cancel: &Cancel, events: &Events, first: bool) {
    batch.status = "Running".into();
    let send_item = |batch: &Batch, index: usize| {
        let _ = events.send(Event::Item {
            index,
            item: Box::new(batch.items[index].clone()),
        });
    };
    let step = |index: usize, lane: Lane, stage: &str, total: u64| -> bool {
        let _ = events.send(Event::Activity {
            lane,
            item: index,
            active: true,
        });
        for tick in 0..=12u64 {
            if cancel.requested() {
                return false;
            }
            crate::model::progress(events, index, stage, total * tick / 12, total);
            std::thread::sleep(Duration::from_millis(45));
        }
        let _ = events.send(Event::Activity {
            lane,
            item: index,
            active: false,
        });
        true
    };
    for &index in indices {
        let size = batch.items[index].source.identity.size;
        let item = &mut batch.items[index];
        item.error = None;
        let archiving = item.archive != Stage::NotRequested;
        if archiving && item.archive != Stage::Verified {
            item.archive = Stage::Copying;
            send_item(batch, index);
            let ok = step(index, Lane::Archive, "Copying original", size)
                && step(index, Lane::Archive, "Checking original", size);
            let item = &mut batch.items[index];
            if !ok {
                item.archive = Stage::Canceled;
                send_item(batch, index);
                break;
            }
            item.archive = Stage::Verified;
            item.archive_evidence = Some(evidence(size));
            send_item(batch, index);
        }
        if batch.items[index].source.editing && batch.items[index].editing != Stage::Verified {
            batch.items[index].editing = Stage::Converting;
            send_item(batch, index);
            let duration = (size as f64 / 12.5) as u64;
            let ok = step(index, Lane::Editing, "Converting audio", duration)
                && step(index, Lane::Editing, "Checking editing checksum", size);
            let item = &mut batch.items[index];
            if !ok {
                item.editing = Stage::Canceled;
                send_item(batch, index);
                break;
            }
            if first && index == 3 {
                item.editing = Stage::Failed;
                item.error = Some("Conversion failed. Inspect details for unsupported streams or damaged media.\nffmpeg failed (exit status: 1). Demo failure.".into());
            } else {
                item.editing = Stage::Verified;
                item.output_evidence = Some(evidence(size * 23 / 20));
            }
            send_item(batch, index);
        }
    }
    for item in &mut batch.items {
        for stage in [&mut item.archive, &mut item.editing] {
            if *stage == Stage::Pending && cancel.requested() {
                *stage = Stage::Canceled;
            }
        }
    }
    batch.status = if batch.items.iter().all(Item::complete) {
        "Completed"
    } else if cancel.requested() {
        "Canceled"
    } else {
        "Needs attention"
    }
    .into();
}
