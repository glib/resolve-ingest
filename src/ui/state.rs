//! Display-independent application state. GTK code sends `Msg`s here and performs the returned
//! `Effect`s; everything the window shows is derived from `Model`, so each screen can be tested
//! without a display.
use super::worker::{Command, DestinationKind, LocationInfo, Reply, Scope};
use crate::{
    catalog,
    diagnostics::human_bytes,
    model::{Batch, Candidate, Event, Item, Lane, Stage},
    settings::{Settings, Verify, Workflow},
};
use chrono::{NaiveDate, NaiveTime};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// A mounted camera card, recognized by its `DCIM` (or Sony `PRIVATE/M4ROOT`) folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Card {
    pub name: String,
    pub path: PathBuf,
    pub can_eject: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub path: PathBuf,
    pub name: String,
    pub workflow: Workflow,
    pub card: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Screen {
    /// Archive or editing destination is missing.
    Setup,
    /// Ready; waiting for a card or a chosen folder.
    Waiting,
    Scanning,
    Review,
    Running,
    Done,
}

/// Where conversion-only outputs go.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Placement {
    #[default]
    Beside,
    EditingFolder,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub enum Preflight {
    #[default]
    Idle,
    Checking,
    Ready,
    Failed(String),
}

/// Display data fixed at scan time, so sorting and grouping never re-parse filenames.
#[derive(Clone, Debug)]
pub struct ClipMeta {
    pub index: usize,
    pub name: String,
    pub day: NaiveDate,
    pub time: NaiveTime,
    pub date_origin: &'static str,
    pub estimated_date: bool,
    pub file_type: String,
    pub size: u64,
    pub duration: Option<f64>,
}
impl ClipMeta {
    fn of(index: usize, c: &Candidate) -> Self {
        let recorded = catalog::recorded(c);
        Self {
            index,
            name: c
                .relative
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            day: recorded.date.date(),
            time: recorded.date.time(),
            date_origin: recorded.origin,
            estimated_date: recorded.origin.starts_with("File"),
            file_type: catalog::file_type(c),
            size: c.identity.size,
            duration: c.probe.as_ref().map(|p| p.duration()).filter(|d| *d > 0.),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Dim,
    Accent,
    Good,
    Warn,
    Bad,
}
#[derive(Clone, Debug, PartialEq)]
pub struct ClipStatus {
    pub text: String,
    pub icon: &'static str,
    pub tone: Tone,
    pub progress: Option<f64>,
    pub tooltip: Option<String>,
}

#[derive(Clone, Debug)]
struct LaneProgress {
    item: usize,
    stage: String,
    bytes: u64,
    total: u64,
    began: Instant,
    first: u64,
}

/// Per-item fractions for the progress bar and estimate.
#[derive(Clone, Debug, Default)]
pub struct Progress {
    started: Option<Instant>,
    lanes: BTreeMap<Lane, LaneProgress>,
    items: HashMap<usize, (f64, f64)>,
}

#[derive(Clone, Debug, Default)]
pub struct Filter {
    pub show_archived: bool,
    pub hidden_types: BTreeSet<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Locations {
    pub archive: Option<LocationInfo>,
    pub editing: Option<LocationInfo>,
}

pub struct Model {
    pub settings: Settings,
    pub screen: Screen,
    pub cards: Vec<Card>,
    pub source: Option<Source>,
    pub candidates: Vec<Candidate>,
    pub meta: Vec<ClipMeta>,
    pub batch: Option<Batch>,
    pub preflight: Preflight,
    generation: u64,
    start_when_ready: bool,
    pub filter: Filter,
    pub placement: Placement,
    pub scan: (usize, String),
    pub progress: Progress,
    pub locations: Locations,
    /// Persistent problem shown in the banner until resolved.
    pub problem: Option<String>,
    pub interrupted: usize,
    /// The worker is running a user-visible command other than a background preflight.
    pub busy: bool,
    pub canceling: bool,
    pub closing: bool,
    pub worker_failed: bool,
}

#[derive(Debug)]
pub enum Msg {
    CardsChanged(Vec<Card>),
    UseSource(PathBuf, Workflow),
    Rescan,
    Toggle(usize, bool),
    ToggleDay(NaiveDate, bool),
    SelectNew,
    SelectNone,
    ShowArchived(bool),
    ShowType(String, bool),
    Placement(Placement),
    ArchiveOnly(usize),
    RetryProbe(usize),
    RunPreflight,
    Start,
    Cancel,
    Retry(Scope),
    PrepareEditing(usize),
    OpenBatch(Box<Batch>),
    Back,
    SetDestination(DestinationKind, PathBuf),
    SetVerify(Verify),
    SetAppearance {
        high_contrast: bool,
        text_scale: f64,
    },
    RequestHistory,
    RequestClose,
    Reply(Reply),
    Events(Vec<Event>, Instant),
}

#[derive(Debug, PartialEq)]
pub enum Effect {
    Send(Command),
    /// Stops the worker's current command (scan or batch); queued commands still run.
    CancelWorker,
    /// Debounced: the window dispatches `Msg::RunPreflight` once selection settles.
    SchedulePreflight,
    RebuildRows,
    RefreshRows,
    RefreshRow(usize),
    SaveSettings,
    ApplyAppearance,
    Toast(String),
    /// A problem the user has to read in full, such as why a folder was refused.
    Alert {
        heading: String,
        body: String,
    },
    Notify {
        title: String,
        body: String,
    },
    ShowHistory(Vec<Batch>),
    ShowPartials {
        batch: String,
        files: Vec<(bool, PathBuf, u64)>,
    },
    Close,
}

impl Model {
    pub fn new(settings: Settings) -> Self {
        let mut model = Self {
            settings,
            screen: Screen::Setup,
            cards: vec![],
            source: None,
            candidates: vec![],
            meta: vec![],
            batch: None,
            preflight: Preflight::Idle,
            generation: 0,
            start_when_ready: false,
            filter: Filter::default(),
            placement: Placement::Beside,
            scan: (0, String::new()),
            progress: Progress::default(),
            locations: Locations::default(),
            problem: None,
            interrupted: 0,
            busy: false,
            canceling: false,
            closing: false,
            worker_failed: false,
        };
        model.screen = model.idle_screen();
        model
    }

    pub fn configured(&self) -> bool {
        self.settings.archive.is_some() && self.settings.editing.is_some()
    }
    /// What the setup screen still needs, in words.
    pub fn setup_hint(&self) -> &'static str {
        match (&self.settings.archive, &self.settings.editing) {
            (None, None) => "Choose both folders to start ingesting cards.",
            (None, Some(_)) => "Choose an archive folder to continue.",
            (Some(_), None) => "Choose an editing copies folder to continue.",
            (Some(_), Some(_)) => "",
        }
    }
    fn idle_screen(&self) -> Screen {
        if self.configured() {
            Screen::Waiting
        } else {
            Screen::Setup
        }
    }
    pub fn workflow(&self) -> Workflow {
        self.source
            .as_ref()
            .map(|s| s.workflow)
            .unwrap_or(Workflow::Ingest)
    }
    /// Rows come from the batch once work has started, otherwise from the scan.
    pub fn showing_batch(&self) -> bool {
        matches!(self.screen, Screen::Running | Screen::Done)
    }
    pub fn rows(&self) -> &[ClipMeta] {
        &self.meta
    }
    pub fn selected(&self) -> impl Iterator<Item = &Candidate> {
        self.candidates.iter().filter(|c| c.selected)
    }
    pub fn selected_bytes(&self) -> u64 {
        self.selected().map(|c| c.identity.size).sum()
    }
    pub fn new_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.archived.is_none() && c.sidecar_for.is_none())
            .count()
    }
    pub fn visible(&self, meta: &ClipMeta) -> bool {
        if self.showing_batch() {
            return true;
        }
        let c = &self.candidates[meta.index];
        (self.filter.show_archived || c.archived.is_none() || c.selected)
            && !self.filter.hidden_types.contains(&meta.file_type)
    }
    pub fn file_types(&self) -> BTreeSet<String> {
        self.meta.iter().map(|m| m.file_type.clone()).collect()
    }
    pub fn archived_count(&self) -> usize {
        self.candidates
            .iter()
            .filter(|c| c.archived.is_some())
            .count()
    }
    /// Selected and total clips on `day`, among visible rows.
    pub fn day_selection(&self, day: NaiveDate) -> (usize, usize, u64) {
        let rows = self.meta.iter().filter(|m| m.day == day && self.visible(m));
        let mut selected = 0;
        let mut total = 0;
        let mut bytes = 0;
        for m in rows {
            total += 1;
            bytes += m.size;
            if self.showing_batch() || self.candidates[m.index].selected {
                selected += 1;
            }
        }
        (selected, total, bytes)
    }
    pub fn can_select(&self) -> bool {
        self.screen == Screen::Review && !self.busy
    }
    pub fn is_selected(&self, index: usize) -> bool {
        if self.showing_batch() {
            true
        } else {
            self.candidates.get(index).is_some_and(|c| c.selected)
        }
    }

    pub fn update(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::CardsChanged(cards) => self.cards_changed(cards),
            Msg::UseSource(path, workflow) => {
                let card = self.cards.iter().find(|c| c.path == path).cloned();
                self.begin_scan(Source {
                    name: card.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| {
                        path.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_else(|| path.display().to_string())
                    }),
                    card: card.is_some() && workflow == Workflow::Ingest,
                    path,
                    workflow,
                })
            }
            Msg::Rescan => match self.source.clone() {
                Some(source) if !matches!(self.screen, Screen::Running | Screen::Scanning) => {
                    self.begin_scan(source)
                }
                _ => vec![],
            },
            Msg::Toggle(index, on) => {
                if !self.can_select() || self.candidates.get(index).is_none_or(|c| c.selected == on)
                {
                    return vec![];
                }
                self.candidates[index].selected = on;
                // Sidecars follow their clip so pairs stay together in the archive.
                let relative = self.candidates[index].relative.clone();
                let mut effects = vec![Effect::RefreshRow(index)];
                for (i, c) in self.candidates.iter_mut().enumerate() {
                    if c.sidecar_for.as_ref() == Some(&relative) && c.selected != on {
                        c.selected = on;
                        effects.push(Effect::RefreshRow(i));
                    }
                }
                self.selection_changed(effects)
            }
            Msg::ToggleDay(day, on) => {
                if !self.can_select() {
                    return vec![];
                }
                let indices: Vec<_> = self
                    .meta
                    .iter()
                    .filter(|m| m.day == day && self.visible(m))
                    .map(|m| m.index)
                    .collect();
                for i in indices {
                    self.candidates[i].selected = on;
                }
                self.selection_changed(vec![Effect::RefreshRows])
            }
            Msg::SelectNew => {
                if !self.can_select() {
                    return vec![];
                }
                let converting = self.workflow() == Workflow::Convert;
                for c in &mut self.candidates {
                    c.selected = if converting {
                        c.editing
                    } else {
                        c.archived.is_none() && default_selected(c)
                    };
                }
                self.selection_changed(vec![Effect::RefreshRows])
            }
            Msg::SelectNone => {
                if !self.can_select() {
                    return vec![];
                }
                for c in &mut self.candidates {
                    c.selected = false;
                }
                self.selection_changed(vec![Effect::RefreshRows])
            }
            Msg::ShowArchived(on) => {
                self.filter.show_archived = on;
                vec![Effect::RefreshRows]
            }
            Msg::ShowType(file_type, on) => {
                if on {
                    self.filter.hidden_types.remove(&file_type);
                } else {
                    self.filter.hidden_types.insert(file_type);
                }
                vec![Effect::RefreshRows]
            }
            Msg::Placement(placement) => {
                if self.placement == placement {
                    return vec![];
                }
                self.placement = placement;
                self.selection_changed(vec![])
            }
            Msg::ArchiveOnly(index) => {
                if !self.can_select() || index >= self.candidates.len() {
                    return vec![];
                }
                let c = &mut self.candidates[index];
                c.editing = false;
                c.selected = true;
                self.selection_changed(vec![Effect::RefreshRow(index)])
            }
            Msg::RetryProbe(index) => match (&self.source, self.candidates.get(index)) {
                (Some(source), Some(c)) if self.can_select() => {
                    self.busy = true;
                    vec![Effect::Send(Command::Probe {
                        index,
                        source: source.path.clone(),
                        candidate: Box::new(c.clone()),
                    })]
                }
                _ => vec![],
            },
            Msg::RunPreflight => self.run_preflight(),
            Msg::Start => self.start(),
            Msg::Cancel => {
                if self.busy {
                    self.canceling = true;
                    return vec![Effect::CancelWorker];
                }
                vec![]
            }
            Msg::Retry(scope) => match &self.batch {
                Some(batch) if self.screen == Screen::Done && !self.busy => {
                    let command = Command::Retry {
                        batch: batch.id.clone(),
                        scope,
                    };
                    self.enter_running();
                    vec![Effect::Send(command), Effect::RefreshRows]
                }
                _ => vec![],
            },
            Msg::PrepareEditing(index) => match &self.batch {
                Some(batch) if self.screen == Screen::Done && !self.busy => {
                    self.busy = true;
                    vec![Effect::Send(Command::PrepareEditing {
                        batch: batch.id.clone(),
                        index,
                    })]
                }
                _ => vec![],
            },
            Msg::OpenBatch(batch) => {
                if self.busy {
                    return vec![];
                }
                self.show_batch(*batch);
                vec![Effect::RebuildRows]
            }
            Msg::Back => {
                if self.busy {
                    return vec![];
                }
                self.reset_session();
                vec![Effect::RebuildRows]
            }
            Msg::SetDestination(kind, path) => {
                if self.busy {
                    return vec![];
                }
                vec![Effect::Send(Command::ValidateDestination { kind, path })]
            }
            Msg::SetVerify(verify) => {
                self.settings.verify = verify;
                let mut effects = vec![Effect::SaveSettings];
                if self.screen == Screen::Review {
                    effects.extend(self.selection_changed(vec![]));
                }
                effects
            }
            Msg::SetAppearance {
                high_contrast,
                text_scale,
            } => {
                self.settings.high_contrast = high_contrast;
                self.settings.text_scale = text_scale;
                vec![Effect::SaveSettings, Effect::ApplyAppearance]
            }
            Msg::RequestHistory => {
                if self.busy || self.worker_failed {
                    return vec![];
                }
                vec![Effect::Send(Command::History)]
            }
            Msg::RequestClose => {
                if self.screen == Screen::Running {
                    self.closing = true;
                    self.canceling = true;
                    vec![Effect::CancelWorker]
                } else {
                    vec![Effect::Close]
                }
            }
            Msg::Reply(reply) => self.reply(reply),
            Msg::Events(events, now) => self.events(events, now),
        }
    }

    fn cards_changed(&mut self, cards: Vec<Card>) -> Vec<Effect> {
        let added: Vec<_> = cards
            .iter()
            .filter(|c| !self.cards.contains(c))
            .cloned()
            .collect();
        self.cards = cards;
        let source_gone = self
            .source
            .as_ref()
            .is_some_and(|s| s.card && !self.cards.iter().any(|c| c.path == s.path));
        if source_gone && matches!(self.screen, Screen::Review | Screen::Scanning) {
            let mut effects = vec![Effect::RebuildRows, Effect::Toast("Card removed".into())];
            if self.screen == Screen::Scanning {
                self.canceling = true;
                effects.push(Effect::CancelWorker);
            }
            self.reset_session();
            return effects;
        }
        // A newly inserted card starts a scan unless work or a review is in progress.
        if let Some(card) = added.into_iter().next()
            && matches!(self.screen, Screen::Waiting | Screen::Done)
            && !self.busy
        {
            return self.update(Msg::UseSource(card.path, Workflow::Ingest));
        }
        vec![]
    }

    fn begin_scan(&mut self, source: Source) -> Vec<Effect> {
        if self.busy || !self.configured() && source.workflow == Workflow::Ingest {
            return vec![];
        }
        self.screen = Screen::Scanning;
        self.scan = (0, String::new());
        self.candidates.clear();
        self.meta.clear();
        self.batch = None;
        self.preflight = Preflight::Idle;
        self.start_when_ready = false;
        self.filter = Filter::default();
        self.busy = true;
        self.canceling = false;
        let command = Command::Scan {
            source: source.path.clone(),
            workflow: source.workflow,
            archive: self.settings.archive.clone(),
        };
        self.source = Some(source);
        vec![Effect::Send(command), Effect::RebuildRows]
    }

    fn reset_session(&mut self) {
        self.screen = self.idle_screen();
        self.source = None;
        self.candidates.clear();
        self.meta.clear();
        self.batch = None;
        self.preflight = Preflight::Idle;
        self.start_when_ready = false;
        self.progress = Progress::default();
    }

    fn selection_changed(&mut self, mut effects: Vec<Effect>) -> Vec<Effect> {
        self.batch = None;
        self.generation += 1;
        self.start_when_ready = false;
        self.preflight = if self.selected().next().is_some() {
            effects.push(Effect::SchedulePreflight);
            Preflight::Checking
        } else {
            Preflight::Idle
        };
        effects
    }

    fn preflight_settings(&self) -> Option<(Settings, bool)> {
        let source = self.source.as_ref()?;
        let mut settings = self.settings.clone();
        settings.source = source.path.clone();
        settings.workflow = source.workflow;
        let beside = source.workflow == Workflow::Convert && self.placement == Placement::Beside;
        if source.workflow == Workflow::Convert {
            settings.archive = None;
        }
        Some((settings, beside))
    }

    fn run_preflight(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Review || self.preflight != Preflight::Checking {
            return vec![];
        }
        let Some((settings, beside)) = self.preflight_settings() else {
            return vec![];
        };
        vec![Effect::Send(Command::Preflight {
            generation: self.generation,
            settings: Box::new(settings),
            candidates: self.candidates.clone(),
            beside,
        })]
    }

    fn start(&mut self) -> Vec<Effect> {
        if self.screen != Screen::Review || self.busy {
            return vec![];
        }
        match (&self.preflight, &self.batch) {
            (Preflight::Ready, Some(batch)) => {
                let command = Command::Start(Box::new(batch.clone()));
                self.enter_running();
                vec![Effect::Send(command), Effect::RebuildRows]
            }
            (Preflight::Checking, _) => {
                self.start_when_ready = true;
                vec![]
            }
            // After fixing a destination (reconnecting a share, granting access), check again.
            (Preflight::Failed(_), _) => self.selection_changed(vec![]),
            _ => vec![],
        }
    }

    fn enter_running(&mut self) {
        self.screen = Screen::Running;
        self.busy = true;
        self.canceling = false;
        self.start_when_ready = false;
        self.progress = Progress::default();
        if let Some(batch) = &self.batch {
            self.meta = batch
                .items
                .iter()
                .enumerate()
                .map(|(i, item)| ClipMeta::of(i, &item.source))
                .collect();
        }
    }

    fn show_batch(&mut self, batch: Batch) {
        self.meta = batch
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| ClipMeta::of(i, &item.source))
            .collect();
        let path = batch.settings.source.clone();
        let workflow = batch.settings.workflow;
        if self.source.as_ref().is_none_or(|s| s.path != path) {
            let card = self.cards.iter().find(|c| c.path == path);
            self.source = Some(Source {
                name: card.map(|c| c.name.clone()).unwrap_or_else(|| {
                    path.file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default()
                }),
                card: card.is_some(),
                path,
                workflow,
            });
        }
        self.batch = Some(batch);
        self.screen = Screen::Done;
        self.progress = Progress::default();
    }

    fn reply(&mut self, reply: Reply) -> Vec<Effect> {
        match reply {
            Reply::Ready { interrupted } => {
                self.interrupted = interrupted;
                self.locations_effects()
            }
            Reply::Failed(error) => {
                self.worker_failed = true;
                self.busy = false;
                self.problem = Some(error);
                vec![]
            }
            Reply::ScanProgress { count, path } => {
                self.scan = (count, path.display().to_string());
                vec![]
            }
            Reply::Scanned { source, result } => {
                self.busy = false;
                let canceled = std::mem::take(&mut self.canceling);
                if self.screen != Screen::Scanning
                    || self.source.as_ref().is_none_or(|s| s.path != source)
                {
                    return vec![];
                }
                match result {
                    Ok(candidates) => {
                        self.meta = candidates
                            .iter()
                            .enumerate()
                            .map(|(i, c)| ClipMeta::of(i, c))
                            .collect();
                        self.candidates = candidates;
                        self.screen = Screen::Review;
                        let effects = self.selection_changed(vec![Effect::RebuildRows]);
                        if self.candidates.is_empty() {
                            self.problem = None;
                        }
                        effects
                    }
                    Err(e) => {
                        self.reset_session();
                        if canceled {
                            vec![Effect::RebuildRows]
                        } else {
                            vec![Effect::RebuildRows, Effect::Toast(first_line(&e))]
                        }
                    }
                }
            }
            Reply::Planned { generation, result } => {
                if generation != self.generation || self.screen != Screen::Review {
                    return vec![];
                }
                match result {
                    Ok(batch) => {
                        self.batch = Some(*batch);
                        self.preflight = Preflight::Ready;
                        if std::mem::take(&mut self.start_when_ready) {
                            return self.start();
                        }
                    }
                    Err(e) => {
                        self.batch = None;
                        self.preflight = Preflight::Failed(e);
                        self.start_when_ready = false;
                    }
                }
                vec![]
            }
            Reply::Finished { batch, error } => {
                self.busy = false;
                self.canceling = false;
                let mut effects = vec![Effect::RefreshRows];
                let title = completion_title(&batch);
                let body = batch.summary();
                self.show_batch(*batch);
                self.problem = error.map(|e| first_line(&e));
                effects.push(Effect::Notify { title, body });
                effects.extend(self.locations_effects());
                if self.closing {
                    effects.push(Effect::Close);
                }
                effects
            }
            Reply::BatchUpdated(batch) => {
                self.busy = false;
                self.show_batch(*batch);
                vec![Effect::RebuildRows]
            }
            Reply::Probed { index, result } => {
                self.busy = false;
                match result {
                    Ok(c) if index < self.candidates.len() => {
                        self.meta[index] = ClipMeta::of(index, &c);
                        self.candidates[index] = *c;
                        self.selection_changed(vec![Effect::RefreshRow(index)])
                    }
                    Ok(_) => vec![],
                    Err(e) => vec![Effect::Toast(first_line(&e))],
                }
            }
            Reply::Destination { kind, result } => match result {
                Ok(destination) => {
                    match kind {
                        DestinationKind::Archive => self.settings.archive = Some(destination),
                        DestinationKind::Editing => self.settings.editing = Some(destination),
                    }
                    if self.screen == Screen::Setup && self.configured() {
                        self.screen = Screen::Waiting;
                    }
                    let mut effects = vec![Effect::SaveSettings];
                    effects.extend(self.locations_effects());
                    if self.screen == Screen::Review {
                        effects.extend(self.selection_changed(vec![]));
                    }
                    effects
                }
                Err(e) => vec![Effect::Alert {
                    heading: "Can't use this folder".into(),
                    body: e,
                }],
            },
            Reply::Locations { archive, editing } => {
                self.locations = Locations { archive, editing };
                vec![]
            }
            Reply::History(batches) => vec![Effect::ShowHistory(batches)],
            Reply::Partials { batch, files } => vec![Effect::ShowPartials { batch, files }],
            Reply::Info(text) => vec![Effect::Toast(text)],
            Reply::Error(e) => {
                self.busy = false;
                self.canceling = false;
                if self.screen == Screen::Running {
                    self.screen = if self.batch.is_some() {
                        Screen::Done
                    } else {
                        self.idle_screen()
                    };
                    self.problem = Some(first_line(&e));
                    return vec![Effect::RefreshRows];
                }
                vec![Effect::Toast(first_line(&e))]
            }
        }
    }

    fn locations_effects(&self) -> Vec<Effect> {
        if self.worker_failed || !self.configured() {
            return vec![];
        }
        vec![Effect::Send(Command::Locations(Box::new(
            self.settings.clone(),
        )))]
    }

    fn events(&mut self, events: Vec<Event>, now: Instant) -> Vec<Effect> {
        let mut effects = Vec::new();
        for event in events {
            match event {
                Event::Activity { lane, item, active } => {
                    if !active
                        && self
                            .progress
                            .lanes
                            .get(&lane)
                            .is_some_and(|p| p.item == item)
                    {
                        self.progress.lanes.remove(&lane);
                        effects.push(Effect::RefreshRow(item));
                    }
                }
                Event::Progress {
                    item,
                    stage,
                    bytes,
                    total,
                } => {
                    if self.screen != Screen::Running {
                        continue;
                    }
                    self.progress.started.get_or_insert(now);
                    let lane = Lane::for_stage(&stage);
                    let p = self
                        .progress
                        .lanes
                        .entry(lane)
                        .or_insert_with(|| LaneProgress {
                            item,
                            stage: stage.clone(),
                            bytes,
                            total,
                            began: now,
                            first: bytes,
                        });
                    if p.item != item || p.stage != stage || bytes < p.bytes {
                        p.began = now;
                        p.first = bytes;
                    }
                    p.item = item;
                    p.stage = stage.clone();
                    p.bytes = bytes;
                    p.total = total;
                    let fraction = if total > 0 {
                        (bytes as f64 / total as f64).clamp(0., 1.)
                    } else {
                        0.
                    };
                    let editing_steps = self.editing_steps();
                    let entry = self.progress.items.entry(item).or_default();
                    match lane {
                        Lane::Archive => {
                            let value = step_fraction(ARCHIVE_STEPS, &stage, fraction);
                            entry.0 = entry.0.max(value);
                        }
                        Lane::LocalCopy | Lane::Editing => {
                            let value = step_fraction(&editing_steps, &stage, fraction);
                            entry.1 = entry.1.max(value);
                        }
                    }
                    if !effects.contains(&Effect::RefreshRow(item)) {
                        effects.push(Effect::RefreshRow(item));
                    }
                }
                Event::Item { index, item } => {
                    if let Some(batch) = &mut self.batch
                        && index < batch.items.len()
                    {
                        batch.items[index] = *item;
                        effects.push(Effect::RefreshRow(index));
                    }
                }
                Event::Message(_) => (),
            }
        }
        effects
    }

    fn editing_steps(&self) -> Vec<&'static str> {
        let Some(batch) = &self.batch else {
            return vec![];
        };
        let mut steps = Vec::new();
        let local = batch.settings.editing.as_ref().is_some_and(|d| !d.shared);
        match batch.settings.workflow {
            Workflow::Convert => steps.push("Checking conversion input"),
            Workflow::Ingest if local => steps.extend(["Copying locally", "Checking local copy"]),
            Workflow::Ingest => (),
        }
        steps.push("Converting audio");
        if batch.settings.verify == Verify::Full {
            steps.push("full decode");
        }
        steps.push("editing checksum");
        steps
    }

    /// Item progress in [0, 1] for each requested half of its work.
    fn item_fractions(&self, index: usize, item: &Item) -> (f64, f64) {
        let live = self.progress.items.get(&index).copied().unwrap_or_default();
        let archive = match item.archive {
            Stage::Verified | Stage::NotRequested => 1.,
            Stage::Failed | Stage::Canceled | Stage::Interrupted => 0.,
            _ => live.0,
        };
        let editing = match item.editing {
            Stage::Verified | Stage::NotRequested => 1.,
            Stage::Failed | Stage::Canceled | Stage::Interrupted => 0.,
            Stage::AwaitingArchive => live.1.max(0.95),
            _ => live.1,
        };
        (archive, editing)
    }

    /// Size-weighted fraction of requested work completed in the current batch.
    pub fn fraction(&self) -> f64 {
        let Some(batch) = &self.batch else {
            return 0.;
        };
        let mut done = 0.;
        let mut total = 0.;
        for (i, item) in batch.items.iter().enumerate() {
            let size = item.source.identity.size.max(1) as f64;
            let (a, e) = self.item_fractions(i, item);
            if item.archive != Stage::NotRequested {
                total += size;
                done += size * a;
            }
            if item.source.editing {
                total += size;
                done += size * e;
            }
        }
        if total == 0. { 0. } else { done / total }
    }

    /// Withheld until enough work has been observed for a stable estimate.
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        let started = self.progress.started?;
        let elapsed = now.duration_since(started).as_secs_f64();
        let fraction = self.fraction();
        if elapsed < 10. || fraction < 0.02 || fraction >= 1. {
            return None;
        }
        Some(Duration::from_secs_f64(
            elapsed * (1. - fraction) / fraction,
        ))
    }

    /// One line per active worker, e.g. "Archive · DJI_0012.MP4 · Checking original · 120 MB/s".
    pub fn activity(&self, now: Instant) -> Vec<String> {
        self.progress
            .lanes
            .iter()
            .map(|(lane, p)| {
                let name = self
                    .meta
                    .get(p.item)
                    .map(|m| m.name.as_str())
                    .unwrap_or("…");
                let rate = now.duration_since(p.began).as_secs_f64();
                let detail = if p.stage.contains("Converting") || p.stage.contains("decode") {
                    String::new()
                } else if rate > 1. {
                    format!(
                        " · {}/s",
                        human_bytes((p.bytes.saturating_sub(p.first) as f64 / rate) as u64)
                    )
                } else {
                    String::new()
                };
                format!("{} · {name} · {}{detail}", lane.label(), p.stage)
            })
            .collect()
    }

    pub fn status(&self, index: usize) -> ClipStatus {
        if self.showing_batch() {
            let Some(item) = self.batch.as_ref().and_then(|b| b.items.get(index)) else {
                return status("", "", Tone::Dim);
            };
            return self.item_status(index, item);
        }
        let Some(c) = self.candidates.get(index) else {
            return status("", "", Tone::Dim);
        };
        let converting = self.workflow() == Workflow::Convert;
        if let Some(path) = &c.archived {
            return ClipStatus {
                tooltip: Some(format!(
                    "Verified copy already in the archive: {}",
                    path.display()
                )),
                ..status("Already archived", "object-select-symbolic", Tone::Dim)
            };
        }
        // Sidecars are archived as-is, so whether ffprobe can read them does not matter.
        if c.sidecar_for.is_some() {
            return status("Sidecar", "", Tone::Dim);
        }
        if let Some(error) = &c.probe_error {
            let text = if converting || !c.selected {
                "Unreadable"
            } else {
                "Original only · unreadable"
            };
            return ClipStatus {
                tooltip: Some(first_line(error)),
                ..status(text, "dialog-warning-symbolic", Tone::Warn)
            };
        }
        if converting {
            return if c.editing {
                status("Video", "", Tone::Neutral)
            } else {
                status("Not video", "", Tone::Dim)
            };
        }
        if c.editing {
            status("New", "", Tone::Accent)
        } else {
            status("Original only", "", Tone::Neutral)
        }
    }

    fn item_status(&self, index: usize, item: &Item) -> ClipStatus {
        let (a, e) = self.item_fractions(index, item);
        let active = self.progress.lanes.values().find(|p| p.item == index);
        let tooltip = item
            .error
            .as_deref()
            .map(first_line)
            .or_else(|| active.map(|p| p.stage.clone()));
        let converting = item.archive == Stage::NotRequested;
        if item.complete() {
            let text = if converting {
                "Converted"
            } else if item.source.editing {
                "Ready to edit"
            } else {
                "Archived"
            };
            return ClipStatus {
                tooltip,
                ..status(text, "object-select-symbolic", Tone::Good)
            };
        }
        if item.error.is_some() || item.archive == Stage::Failed || item.editing == Stage::Failed {
            let text = if item.archive == Stage::Verified {
                "Archived · copy failed"
            } else {
                "Failed"
            };
            return ClipStatus {
                tooltip,
                ..status(text, "dialog-error-symbolic", Tone::Bad)
            };
        }
        if [&item.archive, &item.editing]
            .iter()
            .any(|s| matches!(s, Stage::Canceled | Stage::Interrupted))
        {
            let text = if item.archive == Stage::Interrupted || item.editing == Stage::Interrupted {
                "Interrupted"
            } else {
                "Canceled"
            };
            return ClipStatus {
                tooltip,
                ..status(text, "", Tone::Warn)
            };
        }
        if item.archive == Stage::Pending
            && matches!(item.editing, Stage::Pending | Stage::NotRequested)
            && active.is_none()
        {
            return status("Queued", "", Tone::Dim);
        }
        let requested = usize::from(!converting) + usize::from(item.source.editing);
        let fraction = (if converting { 0. } else { a } + if item.source.editing { e } else { 0. })
            / requested.max(1) as f64;
        let text = match active {
            Some(p) => short_stage(&p.stage).to_string(),
            None if item.editing == Stage::AwaitingArchive => "Waiting for original".into(),
            None if item.archive == Stage::Verified => "Original verified".into(),
            None => item.editing.to_string(),
        };
        ClipStatus {
            progress: Some(fraction),
            tooltip,
            ..status(&text, "", Tone::Accent)
        }
    }

    /// Text for the review bar: headline, detail, and whether Start is available.
    pub fn review_summary(&self) -> (String, String, bool) {
        let count = self.selected().count();
        let converting = self.workflow() == Workflow::Convert;
        if count == 0 {
            let detail = if self.candidates.is_empty() {
                "No footage found".into()
            } else if self.archived_count() == self.candidates.len() {
                "Everything here is already archived".into()
            } else {
                "Select clips to continue".into()
            };
            return ("Nothing selected".into(), detail, false);
        }
        let bytes = self.selected_bytes();
        let headline = format!(
            "{} {} · {}",
            count,
            if count == 1 { "file" } else { "files" },
            human_bytes(bytes)
        );
        let detail = match (&self.preflight, &self.batch) {
            (Preflight::Ready, Some(batch)) if converting => {
                format!(
                    "About {} of converted files",
                    human_bytes(batch.editing_bytes)
                )
            }
            (Preflight::Ready, Some(batch)) => {
                let editing = batch.items.iter().filter(|i| i.source.editing).count();
                format!(
                    "{} to the archive · {} editing {} (about {})",
                    human_bytes(batch.archive_bytes),
                    editing,
                    if editing == 1 { "copy" } else { "copies" },
                    human_bytes(batch.editing_bytes),
                )
            }
            (Preflight::Failed(e), _) => first_line(e),
            _ => "Checking destinations and space…".into(),
        };
        (headline, detail, true)
    }

    pub fn start_label(&self) -> String {
        let count = self.selected().count();
        let verb = if self.workflow() == Workflow::Convert {
            "Convert"
        } else {
            "Ingest"
        };
        if matches!(self.preflight, Preflight::Failed(_)) {
            return "Check Again".into();
        }
        match count {
            0 => verb.into(),
            1 => format!("{verb} 1 File"),
            n => format!("{verb} {n} Files"),
        }
    }
    pub fn visible_count(&self) -> usize {
        self.meta.iter().filter(|m| self.visible(m)).count()
    }

    pub fn done_summary(&self) -> (String, String) {
        let Some(batch) = &self.batch else {
            return (String::new(), String::new());
        };
        let failed = batch.items.iter().filter(|i| !i.complete()).count();
        let ready = batch
            .items
            .iter()
            .filter(|i| i.complete() && i.source.editing)
            .count();
        let originals = batch
            .items
            .iter()
            .filter(|i| i.complete() && i.archive == Stage::Verified)
            .count();
        let converting = batch.settings.workflow == Workflow::Convert;
        let detail = if converting {
            format!("{ready} converted · sources unchanged")
        } else {
            let archive = batch
                .settings
                .archive
                .as_ref()
                .map(|d| d.name())
                .unwrap_or_default();
            format!("{originals} originals verified in {archive} · {ready} ready to edit")
        };
        (
            completion_title(batch),
            if failed > 0 {
                format!("{detail} · {failed} need attention")
            } else {
                detail
            },
        )
    }
    pub fn failed_count(&self) -> usize {
        self.batch
            .as_ref()
            .map(|b| b.items.iter().filter(|i| !i.complete()).count())
            .unwrap_or(0)
    }
    pub fn source_card(&self) -> Option<&Card> {
        let source = self.source.as_ref()?;
        self.cards.iter().find(|c| c.path == source.path)
    }
}

const ARCHIVE_STEPS: &[&str] = &["Copying original", "original"];

/// Position within an ordered list of stage markers (matched by substring).
fn step_fraction(steps: &[&str], stage: &str, fraction: f64) -> f64 {
    let Some(position) = steps.iter().position(|s| stage.contains(s)) else {
        return 0.;
    };
    (position as f64 + fraction) / steps.len() as f64
}

fn short_stage(stage: &str) -> &str {
    if stage.contains("Copying original") {
        "Copying"
    } else if stage.contains("original") {
        "Checking original"
    } else if stage.contains("Copying locally") {
        "Copying locally"
    } else if stage.contains("local copy") || stage.contains("input") {
        "Checking source"
    } else if stage.contains("Converting") {
        "Converting audio"
    } else {
        "Checking copy"
    }
}

fn status(text: &str, icon: &'static str, tone: Tone) -> ClipStatus {
    ClipStatus {
        text: text.into(),
        icon,
        tone,
        progress: None,
        tooltip: None,
    }
}

fn default_selected(c: &Candidate) -> bool {
    let ext = c
        .relative
        .extension()
        .unwrap_or_default()
        .to_string_lossy()
        .to_ascii_lowercase();
    c.editing
        || c.sidecar_for.is_some()
        || matches!(
            ext.as_str(),
            "mp4" | "mov" | "mxf" | "mts" | "m2ts" | "avi" | "mkv" | "lrf"
        )
}

fn completion_title(batch: &Batch) -> String {
    let total = batch.items.len();
    let done = batch.items.iter().filter(|i| i.complete()).count();
    let converting = batch.settings.workflow == Workflow::Convert;
    let noun = if total == 1 { "file" } else { "files" };
    if batch.status == "Canceled" {
        format!("Canceled · {done} of {total} {noun} finished")
    } else if done == total {
        if converting {
            format!("{total} {noun} converted")
        } else {
            format!("{total} {noun} ingested")
        }
    } else {
        format!("{done} of {total} {noun} finished")
    }
}

pub fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or(text).to_string()
}

pub fn remaining_text(d: Duration) -> String {
    let minutes = (d.as_secs_f64() / 60.).round() as u64;
    match minutes {
        0 => "less than a minute left".into(),
        1..=59 => format!("about {minutes} min left"),
        _ => format!("about {} h {} min left", minutes / 60, minutes % 60),
    }
}

/// The folder a location chip opens and describes.
pub fn location_path(model: &Model, kind: DestinationKind) -> Option<&Path> {
    let settings = model
        .batch
        .as_ref()
        .map(|b| &b.settings)
        .unwrap_or(&model.settings);
    match kind {
        DestinationKind::Archive => settings.archive.as_ref().map(|d| d.path.as_path()),
        DestinationKind::Editing => settings.editing.as_ref().map(|d| d.path.as_path()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{Evidence, Identity},
        settings::Destination,
    };

    fn destination(name: &str) -> Destination {
        Destination {
            path: PathBuf::from("/mnt").join(name),
            expected_mount: "ext4|/dev/x|/".into(),
            shared: false,
        }
    }
    fn configured() -> Model {
        Model::new(Settings {
            archive: Some(destination("archive")),
            editing: Some(destination("editing")),
            ..Default::default()
        })
    }
    fn clip(name: &str, size: u64) -> Candidate {
        Candidate {
            relative: PathBuf::from("DCIM").join(name),
            identity: Identity {
                size,
                ..Default::default()
            },
            selected: true,
            editing: true,
            fingerprint: Some(name.into()),
            ..Default::default()
        }
    }
    fn card() -> Card {
        Card {
            name: "DJI Action".into(),
            path: "/run/media/me/DJI".into(),
            can_eject: true,
        }
    }
    fn sent(effects: &[Effect]) -> Vec<&Command> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Send(c) => Some(c),
                _ => None,
            })
            .collect()
    }
    fn scanned(model: &mut Model, candidates: Vec<Candidate>) -> Vec<Effect> {
        let source = model.source.as_ref().unwrap().path.clone();
        model.update(Msg::Reply(Reply::Scanned {
            source,
            result: Ok(candidates),
        }))
    }
    fn plan(model: &Model) -> Batch {
        Batch {
            schema: 1,
            id: "batch".into(),
            created_at: String::new(),
            settings: model.settings.clone(),
            profile: String::new(),
            tools: None,
            items: model
                .selected()
                .map(|c| Item {
                    source: c.clone(),
                    archive_path: c.relative.clone(),
                    editing_path: Some(c.relative.with_extension("mov")),
                    archive: Stage::Pending,
                    editing: Stage::Pending,
                    archive_evidence: None,
                    input_evidence: None,
                    output_evidence: None,
                    attempts: vec![],
                    error: None,
                    notices: vec![],
                })
                .collect(),
            skipped: 0,
            status: "Ready".into(),
            archive_bytes: 300,
            editing_bytes: 400,
            storage_notes: vec![],
        }
    }

    #[test]
    fn first_run_needs_both_destinations_before_waiting_for_cards() {
        let mut m = Model::new(Settings::default());
        assert_eq!(m.screen, Screen::Setup);
        assert!(m.update(Msg::CardsChanged(vec![card()])).is_empty());
        assert_eq!(m.screen, Screen::Setup, "no scan before setup");
        m.update(Msg::Reply(Reply::Destination {
            kind: DestinationKind::Archive,
            result: Ok(destination("a")),
        }));
        assert_eq!(m.screen, Screen::Setup);
        assert_eq!(
            m.setup_hint(),
            "Choose an editing copies folder to continue."
        );
        let effects = m.update(Msg::Reply(Reply::Destination {
            kind: DestinationKind::Editing,
            result: Ok(destination("e")),
        }));
        assert_eq!(m.screen, Screen::Waiting);
        assert!(effects.contains(&Effect::SaveSettings));
    }

    #[test]
    fn a_refused_folder_is_explained_in_full() {
        let mut m = Model::new(Settings::default());
        let effects = m.update(Msg::Reply(Reply::Destination {
            kind: DestinationKind::Archive,
            result: Err("Not a mounted share.\nMount it first.".into()),
        }));
        assert_eq!(m.screen, Screen::Setup);
        assert!(
            matches!(&effects[..], [Effect::Alert { body, .. }] if body.contains("Mount it first"))
        );
        assert_eq!(
            m.setup_hint(),
            "Choose both folders to start ingesting cards."
        );
    }

    #[test]
    fn inserting_a_card_scans_it_and_preselects_only_new_clips() {
        let mut m = configured();
        let effects = m.update(Msg::CardsChanged(vec![card()]));
        assert_eq!(m.screen, Screen::Scanning);
        assert!(matches!(sent(&effects)[..], [Command::Scan { .. }]));
        let mut old = clip("old.MP4", 100);
        old.archived = Some("2026-08-01/old.MP4".into());
        old.selected = false;
        let effects = scanned(&mut m, vec![old, clip("new.MP4", 200)]);
        assert_eq!(m.screen, Screen::Review);
        assert!(effects.contains(&Effect::SchedulePreflight));
        assert_eq!(m.new_count(), 1);
        assert_eq!(m.status(0).text, "Already archived");
        assert!(
            !m.visible(&m.meta[0].clone()),
            "archived clips hidden by default"
        );
        m.update(Msg::ShowArchived(true));
        assert!(m.visible(&m.meta[0].clone()));
        assert_eq!(m.start_label(), "Ingest 1 File");
    }

    #[test]
    fn start_waits_for_the_latest_preflight_and_ignores_stale_ones() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 100), clip("b.MP4", 200)]);
        let stale = plan(&m);
        m.update(Msg::Toggle(1, false));
        let effects = m.update(Msg::RunPreflight);
        let Command::Preflight { generation, .. } = sent(&effects)[0] else {
            panic!("expected preflight");
        };
        let generation = *generation;
        assert!(m.update(Msg::Start).is_empty(), "start is deferred");
        m.update(Msg::Reply(Reply::Planned {
            generation: generation - 1,
            result: Ok(Box::new(stale)),
        }));
        assert_eq!(m.screen, Screen::Review, "stale plan ignored");
        let effects = m.update(Msg::Reply(Reply::Planned {
            generation,
            result: Ok(Box::new(plan(&m))),
        }));
        assert_eq!(m.screen, Screen::Running);
        let Command::Start(batch) = sent(&effects)[0] else {
            panic!("expected start");
        };
        assert_eq!(batch.items.len(), 1);
    }

    #[test]
    fn preflight_errors_block_start_and_offer_a_recheck() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 100)]);
        m.update(Msg::Reply(Reply::Planned {
            generation: m.generation,
            result: Err("Insufficient archive space\nmore".into()),
        }));
        let (_, detail, _) = m.review_summary();
        assert_eq!(detail, "Insufficient archive space");
        assert_eq!(m.start_label(), "Check Again");
        assert_eq!(m.update(Msg::Start), vec![Effect::SchedulePreflight]);
        assert_eq!(m.preflight, Preflight::Checking);
        assert_eq!(
            m.screen,
            Screen::Review,
            "no batch starts from a failed check"
        );
    }

    #[test]
    fn sidecars_follow_their_clip_and_days_toggle_together() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        let mut sidecar = clip("a.LRF", 10);
        sidecar.editing = false;
        sidecar.sidecar_for = Some("DCIM/a.MP4".into());
        scanned(&mut m, vec![clip("a.MP4", 100), sidecar, clip("b.MP4", 5)]);
        m.update(Msg::Toggle(0, false));
        assert!(!m.candidates[1].selected);
        let day = m.meta[0].day;
        assert_eq!(m.day_selection(day).0, 1);
        m.update(Msg::ToggleDay(day, true));
        assert_eq!(m.day_selection(day), (3, 3, 115));
        m.update(Msg::SelectNone);
        assert_eq!(m.selected().count(), 0);
        assert_eq!(m.review_summary().0, "Nothing selected");
    }

    #[test]
    fn progress_is_size_weighted_and_estimate_waits_for_evidence() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 300), clip("b.MP4", 100)]);
        m.batch = Some(plan(&m));
        m.preflight = Preflight::Ready;
        m.update(Msg::Start);
        let t0 = Instant::now();
        let copy = |bytes| Event::Progress {
            item: 0,
            stage: "Copying original".into(),
            bytes,
            total: 300,
        };
        m.update(Msg::Events(vec![copy(300)], t0));
        // Item 0 archive half-done (copy, not yet checked): 0.5 * 300 of 800 units.
        assert!((m.fraction() - 150. / 800.).abs() < 1e-9);
        assert_eq!(m.remaining(t0 + Duration::from_secs(5)), None);
        let left = m.remaining(t0 + Duration::from_secs(30)).unwrap();
        assert_eq!(left.as_secs(), 130);
        assert_eq!(m.status(0).text, "Copying");
        assert_eq!(m.status(1).text, "Queued");
        let mut done = m.batch.as_ref().unwrap().items[1].clone();
        done.archive = Stage::Verified;
        done.editing = Stage::Verified;
        done.archive_evidence = Some(Evidence {
            sha256: String::new(),
            bytes: 100,
            checked_at: String::new(),
            checks: vec![],
        });
        m.update(Msg::Events(
            vec![Event::Item {
                index: 1,
                item: Box::new(done),
            }],
            t0,
        ));
        assert_eq!(m.status(1).text, "Ready to edit");
        assert!((m.fraction() - 350. / 800.).abs() < 1e-9);
    }

    #[test]
    fn finishing_notifies_and_failures_offer_retry() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 300)]);
        m.batch = Some(plan(&m));
        m.preflight = Preflight::Ready;
        m.update(Msg::Start);
        let mut batch = m.batch.clone().unwrap();
        batch.items[0].archive = Stage::Verified;
        batch.items[0].editing = Stage::Failed;
        batch.items[0].error = Some("Conversion failed\nstderr".into());
        batch.status = "Needs attention".into();
        let effects = m.update(Msg::Reply(Reply::Finished {
            batch: Box::new(batch),
            error: None,
        }));
        assert_eq!(m.screen, Screen::Done);
        assert!(effects.iter().any(|e| matches!(e, Effect::Notify { .. })));
        assert_eq!(m.failed_count(), 1);
        assert_eq!(m.status(0).text, "Archived · copy failed");
        assert_eq!(m.done_summary().0, "0 of 1 file finished");
        let effects = m.update(Msg::Retry(Scope::Incomplete));
        assert_eq!(m.screen, Screen::Running);
        assert!(matches!(
            sent(&effects)[..],
            [Command::Retry {
                scope: Scope::Incomplete,
                ..
            }]
        ));
    }

    #[test]
    fn removing_the_card_during_review_returns_to_waiting() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 300)]);
        let effects = m.update(Msg::CardsChanged(vec![]));
        assert_eq!(m.screen, Screen::Waiting);
        assert!(effects.contains(&Effect::Toast("Card removed".into())));
        assert!(m.candidates.is_empty());
    }

    #[test]
    fn closing_during_work_cancels_first() {
        let mut m = configured();
        m.update(Msg::CardsChanged(vec![card()]));
        scanned(&mut m, vec![clip("a.MP4", 300)]);
        m.batch = Some(plan(&m));
        m.preflight = Preflight::Ready;
        m.update(Msg::Start);
        assert_eq!(m.update(Msg::RequestClose), vec![Effect::CancelWorker]);
        assert!(m.canceling);
        let batch = m.batch.clone().unwrap();
        let effects = m.update(Msg::Reply(Reply::Finished {
            batch: Box::new(batch),
            error: None,
        }));
        assert!(effects.contains(&Effect::Close));
    }

    #[test]
    fn convert_workflow_uses_its_own_labels_and_placement() {
        let mut m = configured();
        m.update(Msg::UseSource("/home/me/Videos".into(), Workflow::Convert));
        assert_eq!(m.source.as_ref().unwrap().name, "Videos");
        scanned(&mut m, vec![clip("a.mp4", 300)]);
        assert_eq!(m.start_label(), "Convert 1 File");
        assert_eq!(m.status(0).text, "Video");
        m.update(Msg::Placement(Placement::EditingFolder));
        let effects = m.update(Msg::RunPreflight);
        let Command::Preflight {
            settings, beside, ..
        } = sent(&effects)[0]
        else {
            panic!("expected preflight");
        };
        assert!(!beside);
        assert!(settings.archive.is_none());
        assert_eq!(settings.workflow, Workflow::Convert);
    }

    #[test]
    fn remaining_time_reads_naturally() {
        assert_eq!(
            remaining_text(Duration::from_secs(20)),
            "less than a minute left"
        );
        assert_eq!(remaining_text(Duration::from_secs(185)), "about 3 min left");
        assert_eq!(
            remaining_text(Duration::from_secs(4900)),
            "about 1 h 22 min left"
        );
    }
}
