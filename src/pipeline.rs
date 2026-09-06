use crate::{
    jobs::Jobs,
    media,
    model::*,
    settings::{Destination, Settings, Workflow},
    storage::{Root, reject_overlap, safe_relative},
    verification,
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub async fn plan(
    settings: Settings,
    candidates: Vec<Candidate>,
    cancel: &Cancel,
) -> Result<Batch> {
    if settings.workflow == Workflow::Convert {
        return plan_conversion(settings, candidates, cancel).await;
    }
    let skipped = candidates.iter().filter(|c| !c.selected).count();
    let selected: Vec<_> = candidates.into_iter().filter(|c| c.selected).collect();
    ensure!(
        !selected.is_empty(),
        "Select at least one original to archive."
    );
    let archive = settings
        .archive
        .as_ref()
        .context("Choose an original archive in Settings.")?;
    let editing_requested = selected.iter().any(|c| c.editing);
    let editing = if editing_requested {
        Some(
            settings
                .editing
                .as_ref()
                .context("Choose an editing destination.")?,
        )
    } else {
        settings.editing.as_ref()
    };
    let mut paths = vec![settings.source.as_path(), archive.path.as_path()];
    if let Some(e) = editing {
        paths.push(&e.path);
    }
    reject_overlap(&paths)?;
    let source = Root::source(&settings.source)?;
    let (ar, archive_note) = checked_destination(archive, "Original archive")?;
    let mut notes = vec![archive_note];
    let ed = editing
        .map(|destination| {
            let (root, note) = checked_destination(destination, "Editing destination")?;
            notes.push(note);
            Ok::<_, anyhow::Error>(root)
        })
        .transpose()?;
    let tools = if editing_requested {
        Some(media::check_tools(cancel).await?)
    } else {
        None
    };
    let id = uuid::Uuid::new_v4().to_string();
    let base = PathBuf::from(chrono::Utc::now().format("%Y-%m-%d").to_string()).join(&id);
    let mut items = Vec::new();
    let mut archive_bytes = 0u64;
    let mut editing_bytes = 0u64;
    for c in selected {
        cancel.check()?;
        ensure!(
            c.relative.to_str().is_some(),
            "Non-UTF8 filenames cannot be journaled in v1: {:?}. Rename a copy before ingest.",
            c.relative
        );
        let file = source.read(&c.relative)?;
        ensure!(
            Identity::of(&file.metadata()?) == c.identity,
            "Source changed since scanning: {}. Rescan the card.",
            c.relative.display()
        );
        let safe = safe_relative(&c.relative)?;
        let archive_path = base.join("originals").join(&safe);
        // Retain the encoded original extension and append .mov: same-stem inputs stay distinct.
        let editing_path = c.editing.then(|| {
            base.join("clips").join(&safe).with_file_name(format!(
                "{}.mov",
                safe.file_name().unwrap().to_string_lossy()
            ))
        });
        ensure!(
            !ar.exists(&archive_path)?,
            "Archive conflict: {}",
            archive_path.display()
        );
        if let Some(p) = &editing_path {
            ensure!(
                !ed.as_ref().unwrap().exists(p)?,
                "Editing conflict: {}",
                p.display()
            );
        }
        archive_bytes = archive_bytes
            .checked_add(c.identity.size)
            .context("Batch size overflow")?;
        if c.editing {
            let probe = c
                .probe
                .as_ref()
                .context("Probe failed. Retry probe or choose Archive only.")?;
            ensure!(
                probe.video().is_some() && probe.duration() > 0.,
                "Unknown/empty media duration. Choose Archive only or retry probe."
            );
            ensure!(
                probe.video().is_some_and(|v| v.pix_fmt.is_some()),
                "The video could not be decoded during probing. Install FFmpeg with this video's decoder, retry probe, or choose Archive only."
            );
            editing_bytes = editing_bytes
                .checked_add(probe.estimate(c.identity.size))
                .context("Batch size overflow")?;
        }
        let notices = c.probe.as_ref().map(|p| p.notices()).unwrap_or_default();
        let editing = if c.editing {
            Stage::Pending
        } else {
            Stage::NotRequested
        };
        items.push(Item {
            source: c,
            archive_path,
            editing_path,
            archive: Stage::Pending,
            editing,
            archive_evidence: None,
            input_evidence: None,
            output_evidence: None,
            attempts: vec![],
            error: None,
            notices,
        });
    }
    let staging_bytes = staging_reserve(&settings, &items);
    if staging_bytes > 0 {
        notes.push(format!("Pipelined local copies: up to {MAX_IN_FLIGHT} originals staged in the editing destination; {staging_bytes} extra bytes reserved. Archive and local reads overlap; one audio conversion runs at a time."));
    }
    space(
        &ar,
        ed.as_ref(),
        archive_bytes,
        editing_bytes.saturating_add(staging_bytes),
    )?;
    Ok(Batch {
        schema: 1,
        id,
        created_at: now(),
        settings,
        profile: media::PROFILE.into(),
        tools,
        items,
        skipped,
        status: "Ready".into(),
        archive_bytes,
        editing_bytes,
        storage_notes: notes,
    })
}
fn checked_destination(destination: &Destination, label: &str) -> Result<(Root, String)> {
    let check = || {
        let root = Root::destination(destination)?;
        let note = root.capabilities()?;
        Ok((root, note))
    };
    check().map_err(|error: anyhow::Error| {
        let hint = match error.downcast_ref::<std::io::Error>() {
            Some(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                if destination.shared {
                    " Give your Linux user write access through the CIFS mount ownership/permissions, and check the NAS account's permission to create, rename, and delete files in this folder."
                } else {
                    " Choose a writable folder in Settings or give your Linux user permission to create, rename, and delete files in this folder."
                }
            }
            Some(e) if e.raw_os_error() == Some(libc::EROFS) => {
                " This filesystem is read-only. Mount it read-write or choose a writable folder in Settings."
            }
            _ => "",
        };
        let message = format!(
            "{label} ‘{}’ failed its storage check.{hint}",
            destination.path.display()
        );
        error.context(message)
    })
}
fn space(ar: &Root, ed: Option<&Root>, archive: u64, editing: u64) -> Result<()> {
    let reserve = 16 * 1024 * 1024;
    if let Some(ed) = ed {
        if ar.same_filesystem(ed) {
            ensure!(
                ar.free_bytes()? >= archive.saturating_add(editing).saturating_add(reserve),
                "Insufficient space on the combined archive/editing filesystem"
            );
        } else {
            ensure!(
                ar.free_bytes()? >= archive.saturating_add(reserve),
                "Insufficient archive space"
            );
            ensure!(
                ed.free_bytes()? >= editing.saturating_add(reserve),
                "Insufficient editing space"
            );
        }
    } else {
        ensure!(
            ar.free_bytes()? >= archive.saturating_add(reserve),
            "Insufficient archive space"
        );
    }
    Ok(())
}
fn record(jobs: &mut Jobs, batch: &Batch, index: usize, events: &Events) -> Result<()> {
    jobs.save_item(batch, index)?;
    let _ = events.send(Event::Item {
        index,
        item: Box::new(batch.items[index].clone()),
    });
    Ok(())
}
fn partial(final_path: &Path, attempt: &str, kind: &str) -> PathBuf {
    final_path
        .parent()
        .unwrap()
        .join(format!(".ri-{attempt}-{kind}.partial"))
}

mod workers;

pub async fn run(
    jobs: &mut Jobs,
    batch: &mut Batch,
    only: Option<usize>,
    cancel: &Cancel,
    events: &Events,
) -> Result<()> {
    run_with_lanes(jobs, batch, only, cancel, events, workers::Lanes::default()).await
}

async fn run_with_lanes(
    jobs: &mut Jobs,
    batch: &mut Batch,
    only: Option<usize>,
    cancel: &Cancel,
    events: &Events,
    lanes: workers::Lanes,
) -> Result<()> {
    use std::collections::VecDeque;
    use workers::{Change, Save, Update, Work};
    ensure!(
        batch.profile == media::PROFILE,
        "This batch uses a different conversion profile. Use its original application version."
    );
    ensure!(
        only.is_none_or(|i| i < batch.items.len()),
        "Item index out of range"
    );
    batch.status = "Running".into();
    jobs.save(batch)?;
    let mut queue: VecDeque<_> = (0..batch.items.len())
        .filter(|i| only.is_none_or(|n| n == *i))
        .map(|i| (i, 0u64))
        .collect();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Update>(8);
    let mut tasks = tokio::task::JoinSet::new();
    let mut blocked = false;
    let mut fatal = None;
    loop {
        while tasks.len() < MAX_IN_FLIGHT && !cancel.requested() && !blocked && fatal.is_none() {
            let Some((index, retry)) = queue.pop_front() else {
                break;
            };
            let item = &mut batch.items[index];
            item.error = None;
            item.archive = if batch.settings.workflow == Workflow::Convert {
                Stage::NotRequested
            } else {
                Stage::Pending
            };
            item.editing = if item.source.editing {
                Stage::Pending
            } else {
                Stage::NotRequested
            };
            let id = uuid::Uuid::new_v4().to_string();
            item.attempts.push(Attempt {
                archive_partial: (batch.settings.workflow == Workflow::Ingest)
                    .then(|| partial(&item.archive_path, &id, "archive")),
                editing_partial: item
                    .editing_path
                    .as_ref()
                    .map(|p| partial(p, &id, "editing")),
                staging_partial: item
                    .editing_path
                    .as_ref()
                    .filter(|_| {
                        batch.settings.workflow == Workflow::Ingest
                            && batch.settings.editing.as_ref().is_some_and(|d| !d.shared)
                    })
                    .map(|p| partial(p, &id, "source")),
                id,
                started_at: now(),
                error: None,
            });
            // Every worker-owned filename is durable before any worker can create it.
            if let Err(e) = record(jobs, batch, index, events) {
                fatal = Some(e);
                cancel.request();
                break;
            }
            let work = Work {
                item: batch.items[index].clone(),
                settings: batch.settings.clone(),
                save: Save {
                    index,
                    tx: tx.clone(),
                },
                lock: jobs.lock_fd(),
                cancel: cancel.clone(),
                events: events.clone(),
            };
            let lanes = lanes.clone();
            tasks.spawn(async move {
                if retry > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(250 * retry)).await;
                }
                let (archive, editing) = workers::run(work, lanes).await;
                (index, retry, archive, editing)
            });
        }
        if tasks.is_empty() {
            break;
        }
        tokio::select! {
            Some(update) = rx.recv() => {
                let result = if fatal.is_some() {
                    Err(anyhow::anyhow!("Journal failed; stopping workers before further publication"))
                } else {
                    let item = &mut batch.items[update.index];
                    match update.change {
                        Change::Archive(stage) => item.archive = stage,
                        Change::Editing(stage) => item.editing = stage,
                        Change::ArchiveEvidence(e) => item.archive_evidence = Some(e),
                        Change::InputEvidence(e) => item.input_evidence = Some(e),
                        Change::OutputEvidence(e) => item.output_evidence = Some(e),
                        Change::Notice(n) => item.notices.push(n),
                        Change::Tools(t) => batch.tools = Some(t),
                    }
                    record(jobs, batch, update.index, events)
                };
                let _ = update.ack.send(result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}")));
                if let Err(e) = result {
                    if fatal.is_none() { fatal = Some(e); }
                    cancel.request();
                }
            }
            Some(result) = tasks.join_next() => {
                match result {
                    Ok((index, retry, archive, editing)) => {
                        let errors: Vec<_> = [archive.as_ref().err(), editing.as_ref().err()].into_iter().flatten().collect();
                        let transient = errors.iter().any(|e| crate::errors::classify(e) == crate::errors::Category::TransientIo);
                        blocked |= errors.iter().any(|e| crate::errors::classify(e) == crate::errors::Category::DestinationUnavailable);
                        let messages: Vec<_> = errors.iter().map(|e| format!("{}\n{e:#}",
                            if cancel.requested() { "Canceled" } else { crate::errors::classify(e).next_step() })).collect();
                        let item = &mut batch.items[index];
                        let failed = if cancel.requested() { Stage::Canceled } else { Stage::Failed };
                        if archive.is_err() { item.archive = failed.clone(); }
                        if editing.is_err() && item.source.editing { item.editing = failed; }
                        if !messages.is_empty() {
                            let message = messages.join("\n");
                            if !cancel.requested() { item.error = Some(message.clone()); }
                            item.attempts.last_mut().unwrap().error = Some(message.clone());
                            let _ = events.send(Event::Message(message));
                        }
                        if let Err(e) = record(jobs, batch, index, events) {
                            if fatal.is_none() { fatal = Some(e); }
                            cancel.request();
                        }
                        if transient && retry < 2 && !cancel.requested() && !blocked {
                            queue.push_back((index, retry + 1));
                        }
                    }
                    Err(e) => {
                        if fatal.is_none() { fatal = Some(anyhow::Error::from(e).context("Pipeline worker stopped unexpectedly")); }
                        cancel.request();
                    }
                }
            }
        }
    }
    // All tasks, blocking filesystem calls, and FFmpeg children have stopped before returning.
    if let Some(error) = fatal {
        return Err(error);
    }
    if cancel.requested() {
        for (index, _) in queue {
            let item = &mut batch.items[index];
            if !matches!(item.archive, Stage::Verified | Stage::NotRequested) {
                item.archive = Stage::Canceled;
            }
            if item.source.editing && item.editing != Stage::Verified {
                item.editing = Stage::Canceled;
            }
            record(jobs, batch, index, events)?;
        }
    }
    batch.status = if batch.items.iter().all(Item::complete) {
        "Completed"
    } else if cancel.requested() {
        "Canceled"
    } else if blocked {
        "Blocked · reconnect destination and retry"
    } else {
        "Needs attention"
    }
    .into();
    jobs.save(batch)?;
    Ok(())
}

pub fn stale_partials(batch: &Batch) -> Result<Vec<(bool, PathBuf, u64)>> {
    let mut found = Vec::new();
    for item in &batch.items {
        for attempt in &item.attempts {
            for (archive, path) in [
                (true, &attempt.archive_partial),
                (false, &attempt.editing_partial),
                (false, &attempt.staging_partial),
            ] {
                if let Some(path) = path {
                    let setting = if archive {
                        &batch.settings.archive
                    } else {
                        &batch.settings.editing
                    };
                    let root = Root::destination(setting.as_ref().context("Missing destination")?)?;
                    if root.exists(path)? {
                        found.push((archive, path.clone(), root.read(path)?.metadata()?.len()));
                    }
                }
            }
        }
    }
    Ok(found)
}
pub fn cleanup(jobs: &Jobs, batch: &Batch) -> Result<u64> {
    // Holding Jobs owns the exclusive lock, including any inherited child lock after a crash.
    let _ = jobs.lock_fd();
    let mut bytes = 0;
    for (archive, path, size) in stale_partials(batch)? {
        let setting = if archive {
            &batch.settings.archive
        } else {
            &batch.settings.editing
        };
        Root::destination(setting.as_ref().unwrap())?.remove_owned(&path)?;
        bytes += size;
    }
    Ok(bytes)
}

/// Explicitly request an editing copy for a previously archive-only item, using its verified archive.
pub async fn prepare_archived_editing(
    jobs: &mut Jobs,
    batch: &mut Batch,
    index: usize,
    cancel: &Cancel,
    events: &Events,
) -> Result<()> {
    ensure!(index < batch.items.len(), "Item index out of range");
    let ar = Root::destination(
        batch
            .settings
            .archive
            .as_ref()
            .context("Missing archive destination")?,
    )?;
    let ed = Root::destination(batch.settings.editing.as_ref().context(
        "This batch has no editing destination. Configure one before creating a new batch.",
    )?)?;
    let item = &batch.items[index];
    let expected = item
        .archive_evidence
        .as_ref()
        .context("Archive must be verified before retrying its probe")?;
    let actual = verification::hash(
        ar.read(&item.archive_path)?,
        cancel,
        events,
        index,
        "Rechecking original",
    )?;
    verification::compare_hash(&actual, expected)?;
    let probe = media::probe(&ar.read(&item.archive_path)?, Some(jobs.lock_fd()), cancel).await?;
    ensure!(
        probe.video().is_some_and(|v| v.pix_fmt.is_some()) && probe.duration() > 0.,
        "No decodable motion video. Keep this file archive only."
    );
    let mut parts = item.archive_path.components();
    let base: PathBuf = parts.by_ref().take(2).collect();
    ensure!(
        parts.next().is_some_and(|c| c.as_os_str() == "originals"),
        "Unrecognized archive path layout"
    );
    let relative: PathBuf = parts.collect();
    let target = base.join("clips").join(&relative).with_file_name(format!(
        "{}.mov",
        relative
            .file_name()
            .context("Missing source filename")?
            .to_string_lossy()
    ));
    ensure!(
        !ed.exists(&target)?,
        "Editing target already exists. Inspect the conflict before requesting conversion."
    );
    space(&ar, Some(&ed), 0, probe.estimate(item.source.identity.size))?;
    batch.tools = Some(media::check_tools(cancel).await?);
    let item = &mut batch.items[index];
    item.source.editing = true;
    item.source.probe_error = None;
    item.notices.extend(probe.notices());
    item.source.probe = Some(probe);
    item.editing_path = Some(target);
    item.editing = Stage::Pending;
    item.error = None;
    batch.status = "Editing requested · ready to retry".into();
    record(jobs, batch, index, events)
}

pub fn copy_file_identity(file: &File) -> Result<Identity> {
    Ok(Identity::of(&file.metadata()?))
}

#[cfg(test)]
mod tests;

async fn plan_conversion(
    mut settings: Settings,
    candidates: Vec<Candidate>,
    cancel: &Cancel,
) -> Result<Batch> {
    let source = Root::source(&settings.source)?;
    let destination = settings
        .editing
        .as_ref()
        .context("Choose where to save converted files")?;
    if settings.source != destination.path {
        reject_overlap(&[&settings.source, &destination.path])?;
    }
    let (editing, note) = checked_destination(destination, "Conversion destination")?;
    let skipped = candidates.iter().filter(|c| !c.selected).count();
    let mut items = Vec::new();
    let mut targets = std::collections::HashSet::new();
    let mut bytes = 0u64;
    for mut c in candidates.into_iter().filter(|c| c.selected) {
        cancel.check()?;
        let probe = c.probe.as_ref().context(
            "A selected file could not be probed. Select video files or retry its probe.",
        )?;
        ensure!(
            probe.video().is_some_and(|v| v.pix_fmt.is_some()) && probe.duration() > 0.,
            "Select decodable video files for conversion: {}",
            c.relative.display()
        );
        ensure!(
            Identity::of(&source.read(&c.relative)?.metadata()?) == c.identity,
            "Source changed since scanning: {}",
            c.relative.display()
        );
        let filename = c
            .relative
            .file_name()
            .and_then(|n| n.to_str())
            .context("Conversion requires a UTF-8 filename")?;
        let path = c.relative.with_file_name(format!("{filename}.mov"));
        ensure!(
            targets.insert(path.to_string_lossy().to_lowercase()),
            "Conversion filenames collide on a case-insensitive destination: {}",
            path.display()
        );
        ensure!(
            !editing.exists(&path)?,
            "Conversion output already exists: {}. Choose another output folder or use History to recheck a recorded conversion.",
            editing.path.join(&path).display()
        );
        bytes = bytes
            .checked_add(probe.estimate(c.identity.size))
            .context("Conversion size overflow")?;
        let notices = probe.notices();
        c.editing = true;
        items.push(Item {
            source: c,
            archive_path: PathBuf::new(),
            editing_path: Some(path),
            archive: Stage::NotRequested,
            editing: Stage::Pending,
            archive_evidence: None,
            input_evidence: None,
            output_evidence: None,
            attempts: vec![],
            error: None,
            notices,
        });
    }
    ensure!(!items.is_empty(), "Select at least one video to convert");
    ensure!(
        editing.free_bytes()? >= bytes.saturating_add(16 * 1024 * 1024),
        "Insufficient space for converted files"
    );
    settings.archive = None;
    Ok(Batch {
        schema: 1, id: uuid::Uuid::new_v4().to_string(), created_at: now(), settings,
        profile: media::PROFILE.into(), tools: Some(media::check_tools(cancel).await?),
        items, skipped, status: "Ready".into(), archive_bytes: 0, editing_bytes: bytes,
        storage_notes: vec![note, "Conversion only: source files stay in place; outputs append .mov to the original filename.".into()],
    })
}
