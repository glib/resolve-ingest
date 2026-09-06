//! Bounded archive, local-copy, and conversion lanes. Only the coordinator writes the journal.
use super::*;
use std::{os::fd::RawFd, sync::Arc};
use tokio::sync::{Semaphore, mpsc, oneshot};

pub(super) enum Change {
    Archive(Stage),
    Editing(Stage),
    ArchiveEvidence(Evidence),
    InputEvidence(Evidence),
    OutputEvidence(Evidence),
    Notice(String),
    Tools(String),
}
pub(super) struct Update {
    pub index: usize,
    pub change: Change,
    pub ack: oneshot::Sender<std::result::Result<(), String>>,
}
#[derive(Clone)]
pub(super) struct Save {
    pub index: usize,
    pub tx: mpsc::Sender<Update>,
}
impl Save {
    async fn apply(&self, change: Change) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .send(Update {
                index: self.index,
                change,
                ack,
            })
            .await
            .context("Journal coordinator stopped")?;
        rx.await
            .context("Journal acknowledgement lost")?
            .map_err(anyhow::Error::msg)
    }
    fn blocking(&self, change: Change) -> Result<()> {
        let (ack, rx) = oneshot::channel();
        self.tx
            .blocking_send(Update {
                index: self.index,
                change,
                ack,
            })
            .context("Journal coordinator stopped")?;
        rx.blocking_recv()
            .context("Journal acknowledgement lost")?
            .map_err(anyhow::Error::msg)
    }
}
#[derive(Clone)]
pub(super) struct Work {
    pub item: Item,
    pub settings: Settings,
    pub save: Save,
    pub lock: RawFd,
    pub cancel: Cancel,
    pub events: Events,
}
#[derive(Clone)]
pub(super) struct Lanes {
    pub archive: Arc<Semaphore>,
    pub local: Arc<Semaphore>,
    pub editing: Arc<Semaphore>,
}
impl Default for Lanes {
    fn default() -> Self {
        Self {
            archive: Arc::new(Semaphore::new(1)),
            local: Arc::new(Semaphore::new(1)),
            editing: Arc::new(Semaphore::new(1)),
        }
    }
}
struct Activity {
    events: Events,
    lane: Lane,
    item: usize,
}
impl Activity {
    fn start(work: &Work, lane: Lane) -> Self {
        let activity = Self {
            events: work.events.clone(),
            lane,
            item: work.save.index,
        };
        let _ = activity.events.send(Event::Activity {
            lane,
            item: activity.item,
            active: true,
        });
        activity
    }
}
impl Drop for Activity {
    fn drop(&mut self) {
        let _ = self.events.send(Event::Activity {
            lane: self.lane,
            item: self.item,
            active: false,
        });
    }
}
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .context("Storage worker stopped unexpectedly")?
}
struct VerifiedArchive {
    evidence: Evidence,
    root: Root,
    identity: Identity,
}
type ArchiveResult = std::result::Result<VerifiedArchive, String>;

pub(super) async fn run(work: Work, lanes: Lanes) -> (Result<()>, Result<()>) {
    if work.settings.workflow == Workflow::Convert {
        let result = async {
            let _permit = lanes.editing.acquire().await?;
            blocking(move || {
                let _activity = Activity::start(&work, Lane::Editing);
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(convert_only(&work))
            })
            .await
        }
        .await;
        return (Ok(()), result);
    }
    let (archive_tx, archive_rx) = oneshot::channel();
    let archive_work = work.clone();
    let archive = async {
        let result = async {
            let _permit = lanes.archive.acquire().await?;
            blocking(move || archive_original(&archive_work)).await
        }
        .await;
        match result {
            Ok(verified) => {
                let _ = archive_tx.send(Ok(verified));
                Ok(())
            }
            Err(e) => {
                let _ = archive_tx.send(Err(format!("{e:#}")));
                Err(e)
            }
        }
    };
    let editing = async {
        if !work.item.source.editing {
            return Ok(());
        }
        editing_copy(work.clone(), lanes.clone(), archive_rx).await
    };
    // Never abandon a blocking writer or supervised child when its sibling fails.
    let (archive, editing) = tokio::join!(archive, editing);
    if archive.is_ok()
        && editing.is_ok()
        && let Some(path) = work
            .item
            .attempts
            .last()
            .and_then(|a| a.staging_partial.clone())
    {
        let w = work.clone();
        if let Err(e) = blocking(move || {
            let root = Root::destination(
                w.settings
                    .editing
                    .as_ref()
                    .context("Missing editing destination")?,
            )?;
            if root.exists(&path)? {
                root.remove_owned(&path)?;
            }
            Ok(())
        })
        .await
        {
            let _ = work
                .save
                .apply(Change::Notice(format!(
                    "Local temporary copy retained; use History cleanup: {e:#}"
                )))
                .await;
        }
    }
    (archive, editing)
}

fn archive_original(w: &Work) -> Result<VerifiedArchive> {
    let _activity = Activity::start(w, Lane::Archive);
    w.cancel.check()?;
    let ar = Root::destination(
        w.settings
            .archive
            .as_ref()
            .context("Missing archive setting")?,
    )?;
    let path = &w.item.archive_path;
    if ar.exists(path)? {
        w.save.blocking(Change::Archive(Stage::Checking))?;
        let expected = w.item.archive_evidence.as_ref().context(
            "Existing archive has no verification evidence. Conflict; nothing overwritten.",
        )?;
        let input = ar.read(path)?;
        let identity = Identity::of(&input.metadata()?);
        let actual = verification::hash(
            input,
            &w.cancel,
            &w.events,
            w.save.index,
            "Rechecking original",
        )?;
        verification::compare_hash(&actual, expected)?;
        ar.validate()?;
        ensure!(
            Identity::of(&ar.read(path)?.metadata()?) == identity,
            "Original changed during verification"
        );
        w.save.blocking(Change::Archive(Stage::Verified))?;
        return Ok(VerifiedArchive {
            evidence: actual,
            root: ar,
            identity,
        });
    }
    w.save.blocking(Change::Archive(Stage::Copying))?;
    let source = Root::source(&w.settings.source)
        .context("Source unavailable. Reconnect the original card or select its current mount.")?;
    let tmp = w
        .item
        .attempts
        .last()
        .and_then(|a| a.archive_partial.as_ref())
        .context("Missing archive attempt path")?;
    let expected = copy_stream(
        w,
        &source,
        &w.item.source.relative,
        &w.item.source.identity,
        &ar,
        tmp,
        "Copying original",
    )?;
    w.save.blocking(Change::Archive(Stage::Checking))?;
    let mut readback = verification::hash(
        ar.read(tmp)?,
        &w.cancel,
        &w.events,
        w.save.index,
        "Checking original",
    )?;
    verification::compare_hash(&readback, &expected)?;
    readback.checks.extend(expected.checks);
    // Publication cannot proceed until this evidence is durably acknowledged.
    w.save.blocking(Change::ArchiveEvidence(readback.clone()))?;
    w.cancel.check()?;
    ar.publish(tmp, path)?;
    let identity = Identity::of(&ar.read(path)?.metadata()?);
    w.save.blocking(Change::Archive(Stage::Verified))?;
    Ok(VerifiedArchive {
        evidence: readback,
        root: ar,
        identity,
    })
}

#[allow(clippy::too_many_arguments)]
fn copy_stream(
    w: &Work,
    source: &Root,
    relative: &Path,
    identity: &Identity,
    target: &Root,
    tmp: &Path,
    stage: &str,
) -> Result<Evidence> {
    let mut input = source.read(relative)?;
    ensure!(
        Identity::of(&input.metadata()?) == *identity,
        "Source removed or changed. Rescan before starting a new batch."
    );
    let mut output = target.create(tmp)?;
    let mut sha = Sha256::new();
    let mut bytes = 0;
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        w.cancel.check()?;
        target.validate()?;
        let n = input
            .read(&mut buffer)
            .context("Cannot read source. Reconnect it and retry.")?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n]).with_context(|| {
            format!(
                "Cannot write {stage} to {}. Check permissions, availability, and free space.",
                target.path.display()
            )
        })?;
        sha.update(&buffer[..n]);
        bytes += n as u64;
        progress(&w.events, w.save.index, stage, bytes, identity.size);
    }
    source.validate()?;
    ensure!(
        bytes == identity.size
            && Identity::of(&input.metadata()?) == *identity
            && Identity::of(&source.read(relative)?.metadata()?) == *identity,
        "Source changed during copy. Rescan the card."
    );
    output.sync_all()?;
    Ok(Evidence {
        sha256: format!("{:x}", sha.finalize()),
        bytes,
        checked_at: now(),
        checks: vec!["Source stream SHA-256 and stable identity/size/timestamps".into()],
    })
}

async fn await_archive(rx: oneshot::Receiver<ArchiveResult>) -> Result<VerifiedArchive> {
    rx.await.context("Archive worker stopped")?.map_err(anyhow::Error::msg)
        .context("Editing publication is waiting for a verified original; retry after resolving the archive error")
}

async fn editing_copy(
    w: Work,
    lanes: Lanes,
    archive_rx: oneshot::Receiver<ArchiveResult>,
) -> Result<()> {
    let check = w.clone();
    let existing = blocking(move || {
        check.cancel.check()?;
        let ed = Root::destination(
            check
                .settings
                .editing
                .as_ref()
                .context("Missing editing destination")?,
        )?;
        ed.exists(
            check
                .item
                .editing_path
                .as_ref()
                .context("Missing editing path")?,
        )
    })
    .await?;
    let local = w.settings.editing.as_ref().is_some_and(|d| !d.shared);
    let mut archive_rx = Some(archive_rx);
    let mut verified_archive = None;
    let mut staged = None;
    if !existing {
        // A retry with archive evidence works without the card, after rechecking the archive.
        if !local || w.item.archive_evidence.is_some() {
            w.save
                .apply(Change::Editing(Stage::AwaitingArchive))
                .await?;
            verified_archive = Some(await_archive(archive_rx.take().unwrap()).await?);
        }
        if local {
            let _permit = lanes.local.acquire().await?;
            let stage_work = w.clone();
            let expected = verified_archive.as_ref().map(|a| a.evidence.clone());
            staged = Some(blocking(move || stage_local(&stage_work, expected.as_ref())).await?);
            w.save.apply(Change::Editing(Stage::Pending)).await?;
        }
    }
    let prepared = {
        let _permit = lanes.editing.acquire().await?;
        let conversion = w.clone();
        let input_proof = staged.clone();
        blocking(move || {
            let _activity = Activity::start(&conversion, Lane::Editing);
            // This runtime lives on a blocking worker: filesystem calls cannot stall the coordinator.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(convert_and_check(
                    &conversion,
                    existing,
                    input_proof.as_ref(),
                ))
        })
        .await?
    };
    w.save
        .apply(Change::Editing(Stage::AwaitingArchive))
        .await?;
    if verified_archive.is_none() {
        verified_archive = Some(await_archive(archive_rx.take().unwrap()).await?);
    }
    let archive = verified_archive.unwrap();
    // Independent card reads must describe exactly the same original, even if both were stable.
    if let Some(staged) = &staged {
        verification::compare_hash(&staged.evidence, &archive.evidence)?;
    }
    blocking(move || {
        let mut evidence = prepared.evidence;
        if staged.is_some() {
            evidence.checks.push(
                "Verified local conversion input SHA-256 matches the verified archive".into(),
            );
        }
        w.save.blocking(Change::OutputEvidence(evidence))?;
        w.cancel.check()?;
        // Recheck the archive mount before exposing a completed editing copy.
        archive.root.validate()?;
        ensure!(
            Identity::of(&archive.root.read(&w.item.archive_path)?.metadata()?) == archive.identity,
            "Verified original changed before editing publication"
        );
        let path = prepared
            .tmp
            .as_ref()
            .unwrap_or(w.item.editing_path.as_ref().unwrap());
        ensure!(
            Identity::of(&prepared.root.read(path)?.metadata()?) == prepared.identity,
            "Verified editing file changed before publication"
        );
        if let Some(tmp) = prepared.tmp {
            prepared
                .root
                .publish(&tmp, w.item.editing_path.as_ref().unwrap())?;
        } else {
            prepared.root.validate()?;
        }
        w.save.blocking(Change::Editing(Stage::Verified))
    })
    .await
}

#[derive(Clone)]
struct Staged {
    evidence: Evidence,
    identity: Identity,
}

fn stage_local(w: &Work, archive: Option<&Evidence>) -> Result<Staged> {
    let _activity = Activity::start(w, Lane::LocalCopy);
    w.cancel.check()?;
    w.save.blocking(Change::Editing(Stage::Staging))?;
    let ed = Root::destination(
        w.settings
            .editing
            .as_ref()
            .context("Missing editing destination")?,
    )?;
    let size = w.item.source.identity.size;
    let estimate = w
        .item
        .source
        .probe
        .as_ref()
        .context("Probe unavailable")?
        .estimate(size);
    ensure!(
        ed.free_bytes()?
            >= size
                .saturating_add(estimate)
                .saturating_add(16 * 1024 * 1024),
        "Insufficient local space for the temporary original and editing copy"
    );
    let tmp = w
        .item
        .attempts
        .last()
        .and_then(|a| a.staging_partial.as_ref())
        .context("Missing local-copy attempt path")?;
    let (source, relative, identity) = if archive.is_some() {
        let ar = Root::destination(w.settings.archive.as_ref().unwrap())?;
        let identity = Identity::of(&ar.read(&w.item.archive_path)?.metadata()?);
        (ar, w.item.archive_path.clone(), identity)
    } else {
        (
            Root::source(&w.settings.source).context("Source unavailable for local copy")?,
            w.item.source.relative.clone(),
            w.item.source.identity.clone(),
        )
    };
    let expected = copy_stream(
        w,
        &source,
        &relative,
        &identity,
        &ed,
        tmp,
        "Copying locally",
    )?;
    let file = ed.read(tmp)?;
    let identity = Identity::of(&file.metadata()?);
    let readback = verification::hash(
        file,
        &w.cancel,
        &w.events,
        w.save.index,
        "Checking local copy",
    )?;
    verification::compare_hash(&readback, &expected)?;
    if let Some(archive) = archive {
        verification::compare_hash(&readback, archive)?;
    }
    ensure!(
        Identity::of(&ed.read(tmp)?.metadata()?) == identity,
        "Local copy changed during verification"
    );
    Ok(Staged {
        evidence: readback,
        identity,
    })
}

struct Prepared {
    root: Root,
    tmp: Option<PathBuf>,
    evidence: Evidence,
    identity: Identity,
}
async fn convert_and_check(w: &Work, existing: bool, staged: Option<&Staged>) -> Result<Prepared> {
    w.cancel.check()?;
    let ed = Root::destination(
        w.settings
            .editing
            .as_ref()
            .context("Missing editing destination")?,
    )?;
    let output_path = w
        .item
        .editing_path
        .as_ref()
        .context("Missing editing path")?;
    let expected = w
        .item
        .source
        .probe
        .as_ref()
        .context("Probe unavailable. Retry probe or archive only.")?;
    let (path, tmp) = if existing {
        w.save.apply(Change::Editing(Stage::Checking)).await?;
        let evidence = w.item.output_evidence.as_ref().context(
            "Existing editing file has no matching evidence. Conflict; nothing overwritten.",
        )?;
        let actual = verification::hash(
            ed.read(output_path)?,
            &w.cancel,
            &w.events,
            w.save.index,
            "Rechecking editing checksum",
        )?;
        verification::compare_hash(&actual, evidence)?;
        (output_path.clone(), None)
    } else {
        ensure!(
            ed.free_bytes()?
                >= expected
                    .estimate(w.item.source.identity.size)
                    .saturating_add(16 * 1024 * 1024),
            "Insufficient editing space"
        );
        w.save
            .apply(Change::Tools(media::check_tools(&w.cancel).await?))
            .await?;
        let tmp = w
            .item
            .attempts
            .last()
            .and_then(|a| a.editing_partial.as_ref())
            .context("Missing editing attempt path")?
            .clone();
        w.save.apply(Change::Editing(Stage::Converting)).await?;
        let input = if w.settings.workflow == Workflow::Convert {
            let source = Root::source(&w.settings.source)?;
            let input = source.read(&w.item.source.relative)?;
            ensure!(
                Identity::of(&input.metadata()?) == w.item.source.identity,
                "Source changed before conversion"
            );
            input
        } else if staged.is_some() {
            ed.read(
                w.item
                    .attempts
                    .last()
                    .and_then(|a| a.staging_partial.as_ref())
                    .context("Missing local copy")?,
            )?
        } else {
            Root::destination(w.settings.archive.as_ref().unwrap())?.read(&w.item.archive_path)?
        };
        let input_identity = Identity::of(&input.metadata()?);
        if let Some(staged) = staged {
            ensure!(
                input_identity == staged.identity,
                "Verified local copy changed before conversion"
            );
        }
        let output = ed.create(&tmp)?;
        let diagnostics = media::convert(
            &input,
            &output,
            expected,
            w.lock,
            &w.cancel,
            &w.events,
            w.save.index,
        )
        .await?;
        ensure!(
            Identity::of(&input.metadata()?) == input_identity,
            "Conversion input changed while reading"
        );
        w.save.apply(Change::Notice(diagnostics)).await?;
        output.sync_all()?;
        (tmp.clone(), Some(tmp))
    };
    ed.validate()?;
    let identity = Identity::of(&ed.read(&path)?.metadata()?);
    w.save.apply(Change::Editing(Stage::Checking)).await?;
    let probe = media::probe(&ed.read(&path)?, Some(w.lock), &w.cancel).await?;
    for notice in verification::structure(expected, &probe)? {
        w.save.apply(Change::Notice(notice)).await?;
    }
    media::decode(
        &ed.read(&path)?,
        &probe,
        w.lock,
        &w.cancel,
        &w.events,
        w.save.index,
    )
    .await?;
    let mut evidence = verification::hash(
        ed.read(&path)?,
        &w.cancel,
        &w.events,
        w.save.index,
        "Checking editing checksum",
    )?;
    ensure!(
        Identity::of(&ed.read(&path)?.metadata()?) == identity,
        "Editing file changed during verification"
    );
    if existing {
        verification::compare_hash(&evidence, w.item.output_evidence.as_ref().unwrap())?;
    }
    evidence.checks.extend([
        "Expected stream manifest, formats, color, rotation, and timing tolerances".into(),
        "Full decode of all expected video/audio streams; not a content-equivalence proof".into(),
    ]);
    w.save
        .apply(Change::OutputEvidence(evidence.clone()))
        .await?;
    Ok(Prepared {
        root: ed,
        tmp,
        evidence,
        identity,
    })
}

async fn convert_only(w: &Work) -> Result<()> {
    w.cancel.check()?;
    let source = Root::source(&w.settings.source)?;
    let input = source.read(&w.item.source.relative)?;
    ensure!(
        Identity::of(&input.metadata()?) == w.item.source.identity,
        "Source changed since scanning; rescan before conversion"
    );
    w.save.apply(Change::Editing(Stage::Checking)).await?;
    let evidence = verification::hash(
        input,
        &w.cancel,
        &w.events,
        w.save.index,
        "Checking conversion input",
    )?;
    let ed = Root::destination(
        w.settings
            .editing
            .as_ref()
            .context("Missing conversion destination")?,
    )?;
    let existing = ed.exists(w.item.editing_path.as_ref().unwrap())?;
    if existing {
        verification::compare_hash(
            &evidence,
            w.item
                .input_evidence
                .as_ref()
                .context("Existing conversion has no source evidence; nothing overwritten")?,
        )?;
    }
    w.save.apply(Change::InputEvidence(evidence)).await?;
    let prepared = convert_and_check(w, existing, None).await?;
    source.validate()?;
    ensure!(
        Identity::of(&source.read(&w.item.source.relative)?.metadata()?) == w.item.source.identity,
        "Source changed during conversion"
    );
    let path = prepared
        .tmp
        .as_ref()
        .unwrap_or(w.item.editing_path.as_ref().unwrap());
    ensure!(
        Identity::of(&prepared.root.read(path)?.metadata()?) == prepared.identity,
        "Converted output changed before publication"
    );
    w.cancel.check()?;
    if let Some(tmp) = prepared.tmp {
        prepared
            .root
            .publish(&tmp, w.item.editing_path.as_ref().unwrap())?;
    }
    w.save.apply(Change::Editing(Stage::Verified)).await
}
