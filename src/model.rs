use serde::{Deserialize, Serialize};
/// Bounds active assets and the number of temporary local originals.
pub const MAX_IN_FLIGHT: usize = 2;
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone, Debug, Default)]
pub struct Cancel(pub Arc<AtomicBool>);
impl Cancel {
    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn check(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.0.load(Ordering::SeqCst),
            "Canceled. Partial files are incomplete; retry when ready."
        );
        Ok(())
    }
    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Stage {
    Pending,
    Copying,
    Staging,
    Checking,
    Converting,
    AwaitingArchive,
    Verified,
    NotRequested,
    Failed,
    Canceled,
    Interrupted,
}
impl std::fmt::Display for Stage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Pending => "Queued",
            Self::Copying => "Copying original",
            Self::Staging => "Copying locally",
            Self::Checking => "Checking",
            Self::Converting => "Converting audio",
            Self::AwaitingArchive => "Waiting for archive verification",
            Self::Verified => "Verified",
            Self::NotRequested => "Archive only",
            Self::Failed => "Failed",
            Self::Canceled => "Canceled",
            Self::Interrupted => "Interrupted",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Identity {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime: i64,
    pub mtime_ns: i64,
    pub ctime: i64,
    pub ctime_ns: i64,
}
impl Identity {
    pub fn of(m: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: m.dev(),
            ino: m.ino(),
            size: m.len(),
            mtime: m.mtime(),
            mtime_ns: m.mtime_nsec(),
            ctime: m.ctime(),
            ctime_ns: m.ctime_nsec(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub relative: PathBuf,
    pub identity: Identity,
    pub probe: Option<crate::media::Probe>,
    pub probe_error: Option<String>,
    pub selected: bool,
    pub editing: bool,
    pub sidecar_for: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub sha256: String,
    pub bytes: u64,
    pub checked_at: String,
    pub checks: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub id: String,
    pub started_at: String,
    pub archive_partial: Option<PathBuf>,
    pub editing_partial: Option<PathBuf>,
    /// Temporary source copy on a local editing destination; never a published original.
    #[serde(default)]
    pub staging_partial: Option<PathBuf>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Item {
    pub source: Candidate,
    pub archive_path: PathBuf,
    pub editing_path: Option<PathBuf>,
    pub archive: Stage,
    pub editing: Stage,
    pub archive_evidence: Option<Evidence>,
    #[serde(default)]
    pub input_evidence: Option<Evidence>,
    pub output_evidence: Option<Evidence>,
    pub attempts: Vec<Attempt>,
    pub error: Option<String>,
    pub notices: Vec<String>,
}
impl Item {
    pub fn complete(&self) -> bool {
        self.error.is_none()
            && matches!(self.archive, Stage::Verified | Stage::NotRequested)
            && matches!(self.editing, Stage::Verified | Stage::NotRequested)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Batch {
    pub schema: u32,
    pub id: String,
    pub created_at: String,
    pub settings: crate::settings::Settings,
    pub profile: String,
    pub tools: Option<String>,
    pub items: Vec<Item>,
    #[serde(default)]
    pub skipped: usize,
    pub status: String,
    pub archive_bytes: u64,
    pub editing_bytes: u64,
    pub storage_notes: Vec<String>,
}
impl Batch {
    pub fn staging_bytes(&self) -> u64 {
        staging_reserve(&self.settings, &self.items)
    }
    pub fn verified_fraction(&self) -> f64 {
        let total: usize = self
            .items
            .iter()
            .map(|i| usize::from(i.archive != Stage::NotRequested) + usize::from(i.source.editing))
            .sum();
        let done: usize = self
            .items
            .iter()
            .map(|i| {
                usize::from(i.archive == Stage::Verified)
                    + usize::from(i.source.editing && i.editing == Stage::Verified)
            })
            .sum();
        if total == 0 {
            0.
        } else {
            done as f64 / total as f64
        }
    }
    pub fn summary(&self) -> String {
        let clips = self
            .items
            .iter()
            .filter(|i| i.complete() && i.source.editing)
            .count();
        let originals = self
            .items
            .iter()
            .filter(|i| i.complete() && !i.source.editing)
            .count();
        let failed = self.items.iter().filter(|i| i.error.is_some()).count();
        let canceled = self
            .items
            .iter()
            .filter(|i| i.archive == Stage::Canceled || i.editing == Stage::Canceled)
            .count();
        if self.settings.workflow == crate::settings::Workflow::Convert {
            return format!(
                "{clips} converted · {failed} failed · {canceled} canceled · {} selected",
                self.items.len()
            );
        }
        format!(
            "{clips} editing clips ready · {originals} archive-only files verified · {failed} failed · {canceled} canceled · {} skipped · {} selected",
            self.skipped,
            self.items.len()
        )
    }
}

pub(crate) fn staging_reserve(settings: &crate::settings::Settings, items: &[Item]) -> u64 {
    if settings.workflow == crate::settings::Workflow::Convert {
        return 0;
    }
    if !settings.editing.as_ref().is_some_and(|d| !d.shared) {
        return 0;
    }
    let mut sizes: Vec<_> = items
        .iter()
        .filter(|i| i.source.editing)
        .map(|i| i.source.identity.size)
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    sizes
        .into_iter()
        .take(MAX_IN_FLIGHT)
        .fold(0, u64::saturating_add)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Archive,
    LocalCopy,
    Editing,
}
impl Lane {
    pub fn for_stage(stage: &str) -> Self {
        if stage.contains("local") {
            Self::LocalCopy
        } else if stage.contains("original") {
            Self::Archive
        } else {
            Self::Editing
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Archive => "Archive",
            Self::LocalCopy => "Local copy",
            Self::Editing => "Editing",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Event {
    Activity {
        lane: Lane,
        item: usize,
        active: bool,
    },
    Progress {
        item: usize,
        stage: String,
        bytes: u64,
        total: u64,
    },
    Item {
        index: usize,
        item: Box<Item>,
    },
    Message(String),
}
pub type Events = std::sync::mpsc::SyncSender<Event>;
pub fn progress(events: &Events, item: usize, stage: &str, bytes: u64, total: u64) {
    let _ = events.try_send(Event::Progress {
        item,
        stage: stage.into(),
        bytes,
        total,
    });
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
