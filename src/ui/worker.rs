//! Background thread that owns the job journal and runs engine commands one at a time.
//! The window polls replies and pipeline events, which coalesces rapid progress updates.
use crate::{
    discovery,
    jobs::Jobs,
    media,
    model::{Batch, Cancel, Candidate, Event, Events},
    pipeline,
    settings::{self, Destination, Settings, Workflow},
    storage::{Root, mount_for},
};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestinationKind {
    Archive,
    Editing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Item(usize),
    Incomplete,
}

/// What a location chip shows: name, storage kind, and free space or the reason it is unusable.
#[derive(Clone, Debug, PartialEq)]
pub struct LocationInfo {
    pub name: String,
    pub kind: &'static str,
    pub free: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Scan {
        source: PathBuf,
        workflow: Workflow,
        archive: Option<Destination>,
    },
    Preflight {
        generation: u64,
        settings: Box<Settings>,
        candidates: Vec<Candidate>,
        beside: bool,
    },
    Start(Box<Batch>),
    Retry {
        batch: String,
        scope: Scope,
    },
    PrepareEditing {
        batch: String,
        index: usize,
    },
    Probe {
        index: usize,
        source: PathBuf,
        candidate: Box<Candidate>,
    },
    ValidateDestination {
        kind: DestinationKind,
        path: PathBuf,
    },
    Locations(Box<Settings>),
    History,
    Export {
        batch: String,
        path: PathBuf,
    },
    Partials(String),
    Cleanup(String),
}

#[derive(Debug)]
pub enum Reply {
    Ready {
        interrupted: usize,
    },
    /// The journal could not be opened; no work is possible.
    Failed(String),
    ScanProgress {
        count: usize,
        path: PathBuf,
    },
    Scanned {
        source: PathBuf,
        result: Result<Vec<Candidate>, String>,
    },
    Planned {
        generation: u64,
        result: Result<Box<Batch>, String>,
    },
    Finished {
        batch: Box<Batch>,
        error: Option<String>,
    },
    BatchUpdated(Box<Batch>),
    Probed {
        index: usize,
        result: Result<Box<Candidate>, String>,
    },
    Destination {
        kind: DestinationKind,
        result: Result<Destination, String>,
    },
    Locations {
        archive: Option<LocationInfo>,
        editing: Option<LocationInfo>,
    },
    History(Vec<Batch>),
    Partials {
        batch: String,
        files: Vec<(bool, PathBuf, u64)>,
    },
    Info(String),
    Error(String),
}

pub struct Worker {
    commands: mpsc::Sender<Command>,
    replies: mpsc::Receiver<Reply>,
    events: mpsc::Receiver<Event>,
    cancel: Arc<Mutex<Cancel>>,
}

impl Worker {
    pub fn spawn(demo: bool) -> Self {
        let (commands, command_rx) = mpsc::channel();
        let (reply_tx, replies) = mpsc::channel();
        let (event_tx, events) = mpsc::sync_channel(256);
        let cancel = Arc::new(Mutex::new(Cancel::default()));
        let current = cancel.clone();
        std::thread::spawn(move || {
            if demo {
                super::demo::run(command_rx, reply_tx, event_tx, current);
            } else {
                run(command_rx, reply_tx, event_tx, current);
            }
        });
        Self {
            commands,
            replies,
            events,
            cancel,
        }
    }
    pub fn send(&self, command: Command) -> bool {
        self.commands.send(command).is_ok()
    }
    /// Cancels the command that is running now; queued commands are unaffected.
    pub fn cancel(&self) {
        self.cancel.lock().unwrap().request();
    }
    pub fn drain(&self) -> (Vec<Reply>, Vec<Event>) {
        // Events first: a reply always follows the events its command produced.
        let events = self.events.try_iter().take(512).collect();
        let replies = self.replies.try_iter().collect();
        (replies, events)
    }
}

/// Next command to run, skipping preflights superseded by a newer one already queued.
pub(super) fn next(rx: &mpsc::Receiver<Command>, queue: &mut VecDeque<Command>) -> Option<Command> {
    if queue.is_empty() {
        queue.push_back(rx.recv().ok()?);
    }
    queue.extend(rx.try_iter());
    loop {
        let command = queue.pop_front()?;
        let superseded = matches!(command, Command::Preflight { .. })
            && queue.iter().any(|c| matches!(c, Command::Preflight { .. }));
        if !superseded {
            return Some(command);
        }
    }
}

pub(super) fn fresh_cancel(current: &Mutex<Cancel>) -> Cancel {
    let cancel = Cancel::default();
    *current.lock().unwrap() = cancel.clone();
    cancel
}

fn run(
    rx: mpsc::Receiver<Command>,
    tx: mpsc::Sender<Reply>,
    events: Events,
    current: Arc<Mutex<Cancel>>,
) {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Reply::Failed(format!("Cannot start worker: {e}")));
            return;
        }
    };
    let mut jobs = match Jobs::open(&settings::data_dir()) {
        Ok(jobs) => jobs,
        Err(e) => {
            let _ = tx.send(Reply::Failed(format!("{e:#}")));
            return;
        }
    };
    let interrupted = jobs
        .history()
        .map(|h| {
            h.iter()
                .filter(|b| b.status.starts_with("Interrupted"))
                .count()
        })
        .unwrap_or(0);
    let _ = tx.send(Reply::Ready { interrupted });
    let mut queue = VecDeque::new();
    while let Some(command) = next(&rx, &mut queue) {
        let cancel = fresh_cancel(&current);
        let reply = runtime.block_on(execute(command, &mut jobs, &cancel, &events, &tx));
        if tx.send(reply).is_err() {
            break;
        }
    }
}

async fn execute(
    command: Command,
    jobs: &mut Jobs,
    cancel: &Cancel,
    events: &Events,
    tx: &mpsc::Sender<Reply>,
) -> Reply {
    let text = |e: anyhow::Error| format!("{e:#}");
    match command {
        Command::Scan {
            source,
            workflow,
            archive,
        } => {
            let result = async {
                let converting = workflow == Workflow::Convert;
                let mut candidates =
                    discovery::scan_with_progress(&source, cancel, converting, |count, path| {
                        let _ = tx.send(Reply::ScanProgress {
                            count,
                            path: path.to_path_buf(),
                        });
                    })
                    .await?;
                if converting {
                    for c in &mut candidates {
                        c.selected = c.editing;
                    }
                } else if let Some(archive) = &archive
                    && let Err(e) = pipeline::mark_archived(jobs, archive, &mut candidates)
                {
                    // An offline archive must not block reviewing the card; preflight reports it.
                    let _ = tx.send(Reply::Info(format!(
                        "Could not check the archive for earlier copies: {}",
                        e.root_cause()
                    )));
                }
                Ok(candidates)
            }
            .await;
            Reply::Scanned {
                source,
                result: result.map_err(text),
            }
        }
        Command::Preflight {
            generation,
            settings,
            candidates,
            beside,
        } => {
            let result = async {
                let mut settings = *settings;
                if beside {
                    settings.editing = Some(Destination::select(settings.source.clone())?);
                }
                Ok(Box::new(
                    pipeline::plan(settings, candidates, cancel).await?,
                ))
            }
            .await;
            Reply::Planned {
                generation,
                result: result.map_err(text),
            }
        }
        Command::Start(mut batch) => {
            if let Err(e) = jobs.save(&batch) {
                return Reply::Error(text(e));
            }
            let error = pipeline::run(jobs, &mut batch, None, cancel, events)
                .await
                .err()
                .map(text);
            Reply::Finished { batch, error }
        }
        Command::Retry { batch, scope } => {
            let mut batch = match jobs.load(&batch) {
                Ok(b) => Box::new(b),
                Err(e) => return Reply::Error(text(e)),
            };
            let result = match scope {
                Scope::Item(i) => pipeline::run(jobs, &mut batch, Some(i), cancel, events).await,
                Scope::Incomplete => {
                    pipeline::retry_incomplete(jobs, &mut batch, cancel, events).await
                }
            };
            Reply::Finished {
                batch,
                error: result.err().map(text),
            }
        }
        Command::PrepareEditing { batch, index } => {
            let result = async {
                let mut batch = jobs.load(&batch)?;
                pipeline::prepare_archived_editing(jobs, &mut batch, index, cancel, events).await?;
                Ok(Box::new(batch))
            }
            .await;
            match result {
                Ok(batch) => Reply::BatchUpdated(batch),
                Err(e) => Reply::Error(text(e)),
            }
        }
        Command::Probe {
            index,
            source,
            mut candidate,
        } => {
            let result = async {
                let file = Root::source(&source)?.read(&candidate.relative)?;
                let probe = media::probe(&file, Some(jobs.lock_fd()), cancel).await?;
                candidate.editing = probe.video().is_some();
                candidate.probe = Some(probe);
                candidate.probe_error = None;
                Ok(candidate)
            }
            .await;
            Reply::Probed {
                index,
                result: result.map_err(text),
            }
        }
        Command::ValidateDestination { kind, path } => Reply::Destination {
            kind,
            result: Destination::select(path).map_err(text),
        },
        Command::Locations(settings) => Reply::Locations {
            archive: settings.archive.as_ref().map(location),
            editing: settings.editing.as_ref().map(location),
        },
        Command::History => match jobs.history() {
            Ok(h) => Reply::History(h),
            Err(e) => Reply::Error(text(e)),
        },
        Command::Export { batch, path } => {
            match jobs
                .load(&batch)
                .and_then(|b| crate::diagnostics::export(&b, &path))
            {
                Ok(()) => Reply::Info(format!("Report saved to {}", path.display())),
                Err(e) => Reply::Error(text(e)),
            }
        }
        Command::Partials(id) => match jobs.load(&id).and_then(|b| pipeline::stale_partials(&b)) {
            Ok(files) => Reply::Partials { batch: id, files },
            Err(e) => Reply::Error(text(e)),
        },
        Command::Cleanup(id) => match jobs.load(&id).and_then(|b| pipeline::cleanup(jobs, &b)) {
            Ok(bytes) => Reply::Info(format!(
                "Removed {} of partial files",
                crate::diagnostics::human_bytes(bytes)
            )),
            Err(e) => Reply::Error(text(e)),
        },
    }
}

pub fn location(d: &Destination) -> LocationInfo {
    let kind = match mount_for(&d.path) {
        Ok(m) if m.is_network() => "Network share",
        Ok(m) if m.point.starts_with("/run/media") || m.point.starts_with("/media") => {
            "External drive"
        }
        _ => "Local disk",
    };
    let (free, error) = match Root::destination(d).and_then(|r| r.free_bytes()) {
        Ok(free) => (Some(free), None),
        Err(e) => (None, Some(format!("{e:#}"))),
    };
    LocationInfo {
        name: d.name(),
        kind,
        free,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preflight(generation: u64) -> Command {
        Command::Preflight {
            generation,
            settings: Box::default(),
            candidates: vec![],
            beside: false,
        }
    }

    #[test]
    fn queued_preflights_collapse_to_the_latest() {
        let (tx, rx) = mpsc::channel();
        for c in [preflight(1), Command::History, preflight(2), preflight(3)] {
            tx.send(c).unwrap();
        }
        let mut queue = VecDeque::new();
        assert_eq!(next(&rx, &mut queue), Some(Command::History));
        assert_eq!(next(&rx, &mut queue), Some(preflight(3)));
        drop(tx);
        assert_eq!(next(&rx, &mut queue), None);
    }
}
