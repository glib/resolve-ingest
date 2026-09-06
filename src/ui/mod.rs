mod table;
use crate::{
    diagnostics::human_bytes,
    discovery,
    jobs::Jobs,
    media,
    model::*,
    pipeline,
    settings::{self, Destination, Settings, Workflow},
};
use adw::prelude::*;
use gtk::{gio, glib};
use std::{cell::RefCell, path::PathBuf, rc::Rc, sync::mpsc, time::Duration};

enum Command {
    PrepareArchivedEditing(String, usize),
    Scan(Settings),
    Plan(Settings, Vec<Candidate>, bool),
    Start(Box<Batch>),
    Retry(String, Option<usize>),
    Select(u8, PathBuf, Settings),
    History,
    Export(String, PathBuf),
    Cleanup(String, bool),
    Probe(usize, Settings, Candidate),
}
enum Reply {
    ScanProgress(usize, PathBuf),
    Scan(Vec<Candidate>),
    Plan(Box<Batch>),
    Finished(Box<Batch>),
    Settings(Settings),
    History(Vec<Batch>),
    Probe(usize, Candidate),
    Error(String),
    Info(String),
}
struct State {
    settings: Settings,
    candidates: Vec<Candidate>,
    batch: Option<Batch>,
    busy: bool,
    ready: bool,
    cancel: Cancel,
    demo: bool,
    closing: bool,
    filter: u32,
    file_type: String,
    since: Option<chrono::NaiveDate>,
    activities: std::collections::BTreeMap<Lane, ActivityProgress>,
}
struct ActivityProgress {
    item: usize,
    stage: String,
    began: std::time::Instant,
    first: u64,
    bytes: u64,
    total: u64,
}
#[derive(Clone)]
struct Row {
    index: usize,
    candidate: Candidate,
    item: Option<Item>,
}
#[derive(Clone)]
struct View {
    window: adw::ApplicationWindow,
    source: gtk::Label,
    archive: gtk::Label,
    editing: gtk::Label,
    banner: gtk::Label,
    summary: gtk::Label,
    stage: gtk::Label,
    progress: gtk::ProgressBar,
    start: gtk::Button,
    cancel: gtk::Button,
    scan: gtk::Button,
    mode: gtk::DropDown,
    placement: gtk::DropDown,
    browse: gtk::Button,
    type_filter: gtk::DropDown,
    bulk_actions: gtk::Box,
    output_column: Rc<RefCell<glib::WeakRef<gtk::ColumnViewColumn>>>,
    settings_window: Rc<RefCell<glib::WeakRef<adw::PreferencesWindow>>>,
    settings_rows: Rc<RefCell<Vec<glib::WeakRef<adw::ActionRow>>>>,
    settings_feedback: Rc<RefCell<glib::WeakRef<gtk::Label>>>,
    settings_appearance: Rc<RefCell<glib::WeakRef<adw::PreferencesGroup>>>,
    output_actions: gtk::FlowBox,
    status_labels: Rc<RefCell<std::collections::HashMap<usize, gtk::Label>>>,
    store: gio::ListStore,
    state: Rc<RefCell<State>>,
    tx: mpsc::Sender<Command>,
}
fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.);
    l.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    if !class.is_empty() {
        l.add_css_class(class);
    }
    l
}
fn ellipsize(l: &gtk::Label) {
    l.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    l.set_hexpand(true);
    l.set_max_width_chars(70);
}
pub fn run(demo: bool) {
    let app = adw::Application::builder()
        .application_id("io.github.resolve_ingest.ResolveIngest")
        .build();
    app.connect_activate(move |app| build(app, demo));
    app.run_with_args(&["resolve-ingest"]);
}
fn worker(rx: mpsc::Receiver<Command>, tx: mpsc::Sender<Reply>, events: Events, cancel: Cancel) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut jobs = match Jobs::open(&settings::data_dir()) {
            Ok(j) => j,
            Err(e) => {
                let _ = tx.send(Reply::Error(format!("{e:#}")));
                return;
            }
        };
        if let Ok(h) = jobs.history() {
            let interrupted: Vec<_> = h.into_iter().filter(|b| b.status != "Completed").collect();
            if !interrupted.is_empty() {
                let _ = tx.send(Reply::History(interrupted));
            }
        }
        while let Ok(command) = rx.recv() {
            let result: anyhow::Result<Reply> = rt.block_on(async {
                Ok(match command {
                    Command::PrepareArchivedEditing(id, index) => { let mut batch = jobs.load(&id)?; pipeline::prepare_archived_editing(&mut jobs, &mut batch, index, &cancel, &events).await?; Reply::Finished(Box::new(batch)) }
                    Command::Scan(s) => {
                        let converting = s.workflow == Workflow::Convert;
                        let mut c = discovery::scan_with_progress(&s.source, &cancel, converting, |count, path| {
                            let _ = tx.send(Reply::ScanProgress(count, path.to_path_buf()));
                        }).await?;
                        if converting { for c in &mut c { c.selected = c.editing; } }
                        Reply::Scan(c)
                    },
                    Command::Plan(mut s, c, alongside) => { if s.workflow == Workflow::Convert && alongside { s.editing = Some(Destination::select(s.source.clone(), false)?); } Reply::Plan(Box::new(pipeline::plan(s, c, &cancel).await?)) },
                    Command::Start(mut b) => { jobs.save(&b)?; pipeline::run(&mut jobs, &mut b, None, &cancel, &events).await?; Reply::Finished(b) }
                    Command::Retry(id, only) => { let mut b = jobs.load(&id)?; pipeline::run(&mut jobs, &mut b, only, &cancel, &events).await?; Reply::Finished(Box::new(b)) }
                    Command::Select(which, path, mut s) => {
                        match which { 0 => s.source = std::fs::canonicalize(path)?, 1 => s.archive = Some(Destination::select(path, true)?), _ => s.editing = Some(Destination::select(path, false)?) }
                        s.save()?; Reply::Settings(s)
                    }
                    Command::History => Reply::History(jobs.history()?),
                    Command::Export(id, path) => { crate::diagnostics::export(&jobs.load(&id)?, &path)?; Reply::Info(format!("Report saved to {}", path.display())) }
                    Command::Cleanup(id, remove) => {
                        let batch = jobs.load(&id)?;
                        if remove { Reply::Info(format!("Removed {} of owned partial files.", human_bytes(pipeline::cleanup(&jobs, &batch)?))) }
                        else { let partials = pipeline::stale_partials(&batch)?; Reply::Info(format!("{} owned partials, {}. Use Remove owned partials in History to clean up. Verified files are retained.", partials.len(), human_bytes(partials.iter().map(|p| p.2).sum()))) }
                    }
                    Command::Probe(i, s, mut c) => {
                        let file = crate::storage::Root::source(&s.source)?.read(&c.relative)?;
                        c.probe = Some(media::probe(&file, Some(jobs.lock_fd()), &cancel).await?); c.probe_error = None;
                        c.editing = c.probe.as_ref().is_some_and(|p| p.video().is_some()); Reply::Probe(i, c)
                    }
                })
            });
            let _ = tx.send(match result {
                Ok(r) => r,
                Err(e) => Reply::Error(format!("{e:#}")),
            });
        }
    });
}
fn build(app: &adw::Application, demo: bool) {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let css = gtk::CssProvider::new();
    css.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    let loaded = Settings::load();
    let error = loaded
        .as_ref()
        .err()
        .map(|e| format!("Settings could not be loaded: {e:#}"));
    let state = Rc::new(RefCell::new(State {
        settings: loaded.unwrap_or_default(),
        candidates: vec![],
        batch: None,
        busy: false,
        ready: false,
        cancel: Cancel::default(),
        demo,
        closing: false,
        filter: 0,
        file_type: String::new(),
        since: None,
        activities: Default::default(),
    }));
    let text_css = gtk::CssProvider::new();
    text_css.load_from_string(&format!(
        "window {{ font-size: {}px; }}",
        state.borrow().settings.text_scale * 15.
    ));
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &text_css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let (tx, commands) = mpsc::channel();
    let (replies, rx) = mpsc::channel();
    let (events, erx) = mpsc::sync_channel(64);
    if !demo {
        worker(commands, replies, events, state.borrow().cancel.clone());
    }
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Resolve Ingest")
        .default_width(1100)
        .default_height(800)
        .build();
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::WindowTitle::new("Resolve Ingest", "")));
    let history = action_button("History");
    let settings_button = action_button("Settings");
    header.pack_end(&settings_button);
    header.pack_end(&history);
    root.append(&header);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.add_css_class("content");
    content.set_vexpand(true);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let mode = gtk::DropDown::from_strings(&["Archive + convert", "Convert existing files"]);
    mode.set_selected(u32::from(
        state.borrow().settings.workflow == Workflow::Convert,
    ));
    mode.update_property(&[gtk::accessible::Property::Label("Workflow")]);
    let placement =
        gtk::DropDown::from_strings(&["Alongside originals", "Use editing destination"]);
    placement.update_property(&[gtk::accessible::Property::Label(
        "Conversion output location",
    )]);
    let browse = action_button("Choose source folder…");
    actions.append(&mode);
    actions.append(&placement);
    actions.append(&browse);
    content.append(&actions);
    let locations = gtk::Box::new(gtk::Orientation::Vertical, 4);
    locations.add_css_class("locations");
    let (source_row, source) = location("Source folder");
    let (archive_row, archive) = location("Original archive");
    let (editing_row, editing) = location("Editing destination");
    locations.append(&source_row);
    locations.append(&archive_row);
    locations.append(&editing_row);
    content.append(&locations);
    let banner = label(
        "Choose a source folder, then scan for footage. Archive and editing locations are managed in Settings.",
        "banner",
    );
    banner.set_wrap(true);
    content.append(&banner);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let clips_title = label("Footage", "heading");
    clips_title.set_hexpand(true);
    toolbar.append(&clips_title);
    let filter = gtk::DropDown::from_strings(&["All files", "Needs attention", "Completed"]);
    filter.set_tooltip_text(Some("Filter clip list"));
    toolbar.append(&filter);
    let scan = action_button("Scan folder");
    toolbar.append(&scan);
    content.append(&toolbar);
    let bulk_actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let type_filter = gtk::DropDown::from_strings(&["All types"]);
    type_filter.update_property(&[gtk::accessible::Property::Label("File type filter")]);
    let since = gtk::Entry::builder()
        .placeholder_text("Since YYYY-MM-DD")
        .width_chars(17)
        .build();
    since.update_property(&[gtk::accessible::Property::Label(
        "Show recordings on or after this date",
    )]);
    let select_shown = action_button("Select shown only");
    let clear_selection = action_button("Clear selection");
    bulk_actions.append(&type_filter);
    bulk_actions.append(&since);
    bulk_actions.append(&select_shown);
    bulk_actions.append(&clear_selection);
    content.append(&bulk_actions);

    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let stage = label("Ready when you are", "heading");
    stage.set_wrap(true);
    let summary = label("No files selected", "muted");
    summary.set_wrap(true);
    let progress = gtk::ProgressBar::new();
    progress.set_tooltip_text(Some("Batch progress counts verified originals and editing copies. Live activity shows each worker's progress."));
    progress.set_show_text(false);
    let start = action_button("Preview output files");
    start.add_css_class("suggested-action");
    let cancel = action_button("Cancel");
    cancel.set_visible(false);
    let output_actions = gtk::FlowBox::new();
    output_actions.set_selection_mode(gtk::SelectionMode::None);
    output_actions.set_column_spacing(8);
    output_actions.set_row_spacing(4);
    output_actions.set_max_children_per_line(4);
    output_actions.set_visible(false);
    let view = View {
        window: window.clone(),
        source,
        editing,
        archive,
        banner,
        summary,
        stage,
        progress,
        start,
        cancel,
        scan,
        mode,
        placement,
        browse,
        type_filter,
        bulk_actions,
        output_column: Rc::new(RefCell::new(glib::WeakRef::new())),
        settings_window: Rc::new(RefCell::new(glib::WeakRef::new())),
        settings_rows: Rc::new(RefCell::new(Vec::new())),
        settings_feedback: Rc::new(RefCell::new(glib::WeakRef::new())),
        settings_appearance: Rc::new(RefCell::new(glib::WeakRef::new())),
        output_actions,
        status_labels: Rc::new(RefCell::new(std::collections::HashMap::new())),
        store: store.clone(),
        state: state.clone(),
        tx,
    };
    let list = table::build(&view);
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vexpand(true)
        .child(&list)
        .min_content_height(120)
        .build();
    content.append(&scroller);
    let body_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();
    root.append(&body_scroll);
    let footer = gtk::Box::new(gtk::Orientation::Vertical, 10);
    footer.add_css_class("footer");
    let footer_row = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let status = gtk::Box::new(gtk::Orientation::Vertical, 4);
    status.set_hexpand(true);
    status.append(&view.stage);
    status.append(&view.summary);
    footer_row.append(&status);
    footer_row.append(&view.cancel);
    footer_row.append(&view.start);
    footer.append(&footer_row);
    footer.append(&view.progress);
    footer.append(&view.output_actions);
    for (title, archive) in [("Open originals", true), ("Open editing folder", false)] {
        let button = action_button(title);
        view.output_actions.insert(&button, -1);
        let v = view.clone();
        button.connect_clicked(move |_| {
            let s = v.state.borrow();
            let Some(batch) = &s.batch else {
                return;
            };
            let path = if archive {
                Some(
                    batch
                        .settings
                        .archive
                        .as_ref()
                        .map(|d| &d.path)
                        .unwrap_or(&batch.settings.source),
                )
            } else {
                batch.settings.editing.as_ref().map(|d| &d.path)
            };
            if let Some(path) = path {
                let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
                let v2 = v.clone();
                launcher.launch(Some(&v.window), gio::Cancellable::NONE, move |r| {
                    if let Err(e) = r {
                        v2.banner.set_text(&format!("Cannot open folder: {e}"));
                    }
                });
            }
        });
    }
    let report = action_button("Export report");
    view.output_actions.insert(&report, -1);
    let v = view.clone();
    report.connect_clicked(move |_| {
        let s = v.state.borrow();
        let Some(batch) = &s.batch else {
            return;
        };
        if s.ready || s.busy {
            v.banner.set_text(
                "Reports are available after the batch has started and the worker has stopped.",
            );
            return;
        }
        let id = batch.id.clone();
        drop(s);
        let dialog = gtk::FileDialog::builder()
            .title("Export batch report")
            .initial_name(format!("resolve-ingest-{id}.json"))
            .build();
        let v2 = v.clone();
        dialog.save(Some(&v.window), gio::Cancellable::NONE, move |r| {
            if let Ok(file) = r
                && let Some(path) = file.path()
            {
                send(&v2, Command::Export(id, path), "Exporting report…");
            }
        });
    });
    let change = action_button("Change selection");
    view.output_actions.insert(&change, -1);
    let v = view.clone();
    change.connect_clicked(move |_| {
        if !v.state.borrow().busy {
            let mut s = v.state.borrow_mut();
            s.batch = None;
            s.ready = false;
            drop(s);
            refresh_rows(&v);
        }
    });
    root.append(&footer);
    {
        let v = view.clone();
        view.browse.connect_clicked(move |_| choose_folder(&v, 0));
    }
    {
        let v = view.clone();
        view.mode.connect_selected_notify(move |mode| {
            let workflow = if mode.selected() == 1 { Workflow::Convert } else { Workflow::Ingest };
            {
                let s = v.state.borrow();
                if s.batch.as_ref().map(|b| b.settings.workflow).unwrap_or(s.settings.workflow) == workflow { return; }
            }
            let mut s = v.state.borrow_mut();
            s.settings.workflow = if mode.selected() == 1 { Workflow::Convert } else { Workflow::Ingest };
            s.batch = None; s.ready = false;
            if s.settings.workflow == Workflow::Convert { for c in &mut s.candidates { if !c.editing { c.selected = false; } } }
            drop(s); refresh_rows(&v);
            v.banner.set_text(if mode.selected() == 1 { "Convert existing videos to MOV with PCM audio. Sources stay unchanged; outputs append .mov to the filename." } else { "Archive originals and create editing copies. Choose files, then preview their output paths." });
        });
    }
    {
        let v = view.clone();
        view.placement.connect_selected_notify(move |_| {
            let mut s = v.state.borrow_mut();
            s.batch = None;
            s.ready = false;
            drop(s);
            refresh_rows(&v);
        });
    }
    {
        let v = view.clone();
        view.type_filter.connect_selected_notify(move |f| {
            let text = f
                .selected_item()
                .and_downcast::<gtk::StringObject>()
                .map(|s| s.string().to_string())
                .unwrap_or_default();
            v.state.borrow_mut().file_type = if text == "All types" {
                String::new()
            } else {
                text
            };
            refresh_rows(&v);
        });
    }
    {
        let v = view.clone();
        since.connect_changed(move |entry| {
            let text = entry.text();
            let date = if text.is_empty() {
                None
            } else if let Ok(date) = chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d") {
                Some(date)
            } else {
                entry.add_css_class("error");
                entry.set_tooltip_text(Some("Use YYYY-MM-DD, for example 2026-08-01"));
                return;
            };
            entry.remove_css_class("error");
            entry.set_tooltip_text(Some(
                "Filter by recording date; * in the table indicates modification-time fallback",
            ));
            v.state.borrow_mut().since = date;
            refresh_rows(&v);
        });
    }
    for (button, select) in [(select_shown, true), (clear_selection, false)] {
        let v = view.clone();
        button.connect_clicked(move |_| {
            if v.state.borrow().busy || v.state.borrow().batch.is_some() {
                return;
            }
            let shown: std::collections::HashSet<_> = (0..v.store.n_items())
                .map(|n| {
                    v.store
                        .item(n)
                        .unwrap()
                        .downcast::<glib::BoxedAnyObject>()
                        .unwrap()
                        .borrow::<Row>()
                        .index
                })
                .collect();
            for (i, c) in v.state.borrow_mut().candidates.iter_mut().enumerate() {
                c.selected = select && shown.contains(&i);
            }
            refresh_rows(&v);
        });
    }
    window.set_content(Some(&root));
    {
        let v = view.clone();
        settings_button.connect_clicked(move |_| settings_dialog(&v));
    }
    {
        let v = view.clone();
        history.connect_clicked(move |_| {
            if !v.state.borrow().busy {
                send(&v, Command::History, "Loading history…");
            }
        });
    }
    {
        let v = view.clone();
        view.scan.connect_clicked(move |_| {
            if v.state.borrow().demo {
                demo_rows(&v);
                return;
            }
            let s = v.state.borrow().settings.clone();
            send(&v, Command::Scan(s), "Scanning source and probing media…");
        });
    }
    {
        let v = view.clone();
        filter.connect_selected_notify(move |f| {
            v.state.borrow_mut().filter = f.selected();
            refresh_rows(&v);
        });
    }
    {
        let v = view.clone();
        view.start.connect_clicked(move |_| {
            if v.state.borrow().demo {
                demo_progress(&v);
                return;
            }
            let s = v.state.borrow();
            if let Some(b) = &s.batch {
                if s.ready {
                    let b = b.clone();
                    drop(s);
                    send(&v, Command::Start(Box::new(b)), "Starting durable ingest…");
                    return;
                }
                let id = b.id.clone();
                drop(s);
                send(
                    &v,
                    Command::Retry(id, None),
                    "Revalidating recorded work and retrying…",
                );
                return;
            }
            let (settings, candidates) = (s.settings.clone(), s.candidates.clone());
            drop(s);
            send(
                &v,
                Command::Plan(settings, candidates, v.placement.selected() == 0),
                "Checking locations, space, and publication support…",
            );
        });
    }
    {
        let v = view.clone();
        view.cancel.connect_clicked(move |_| {
            v.state.borrow().cancel.request();
            v.stage
                .set_text("Cancellation pending · waiting for the active worker to stop");
            v.cancel.set_sensitive(false);
        });
    }
    {
        let v = view.clone();
        window.connect_close_request(move |_| {
        if !v.state.borrow().busy { return glib::Propagation::Proceed; }
        let dialog = adw::MessageDialog::new(Some(&v.window), Some("Ingest is still active"), Some("Canceling waits for the worker to stop. A stalled share may take longer to respond."));
        dialog.add_responses(&[("keep", "Keep ingesting"), ("cancel", "Cancel and close")]); dialog.set_default_response(Some("keep")); dialog.set_close_response("keep");
        let v = v.clone(); dialog.connect_response(None, move |_, response| { if response == "cancel" { let mut s = v.state.borrow_mut(); s.closing = true; s.cancel.request(); v.stage.set_text("Cancellation pending · window closes when work stops"); } }); dialog.present(); glib::Propagation::Stop
    });
    }
    let v = view.clone();
    glib::timeout_add_local(Duration::from_millis(100), move || {
        for event in erx.try_iter().take(128) {
            match event {
                Event::Activity { lane, item, active } => {
                    if !v.state.borrow().busy {
                        continue;
                    }
                    let mut s = v.state.borrow_mut();
                    if active {
                        s.activities.insert(
                            lane,
                            ActivityProgress {
                                item,
                                stage: "Starting…".into(),
                                began: std::time::Instant::now(),
                                first: 0,
                                bytes: 0,
                                total: 0,
                            },
                        );
                    } else if s.activities.get(&lane).is_some_and(|p| p.item == item) {
                        s.activities.remove(&lane);
                    }
                    drop(s);
                    refresh_activity(&v);
                }
                Event::Progress {
                    item,
                    stage,
                    bytes,
                    total,
                } => {
                    if !v.state.borrow().busy {
                        continue;
                    }
                    let mut s = v.state.borrow_mut();
                    let lane = Lane::for_stage(&stage);
                    let p = s
                        .activities
                        .entry(lane)
                        .or_insert_with(|| ActivityProgress {
                            item,
                            stage: stage.clone(),
                            began: std::time::Instant::now(),
                            first: bytes,
                            bytes,
                            total,
                        });
                    if p.item != item || p.stage != stage || bytes < p.bytes {
                        p.began = std::time::Instant::now();
                        p.first = bytes;
                    }
                    p.item = item;
                    p.stage = stage;
                    p.bytes = bytes;
                    p.total = total;
                    drop(s);
                    refresh_activity(&v);
                }
                Event::Item { index, item } => {
                    if let Some(b) = &mut v.state.borrow_mut().batch
                        && index < b.items.len()
                    {
                        b.items[index] = *item;
                    }
                    update_row(&v, index);
                    refresh_activity(&v);
                    if let Some(b) = &v.state.borrow().batch {
                        v.summary.set_text(&b.summary());
                    }
                }
                Event::Message(m) => v.banner.set_text(m.lines().next().unwrap_or(&m)),
            }
        }
        for reply in rx.try_iter() {
            if let Reply::ScanProgress(count, path) = reply {
                if !v.state.borrow().cancel.requested() {
                    v.stage.set_text(&format!(
                        "Scanning · {count} files checked · {}",
                        path.display()
                    ));
                    v.progress.pulse();
                }
                continue;
            }
            v.state.borrow_mut().busy = false;
            v.state.borrow_mut().activities.clear();
            // The reply follows all worker events. Discard any that arrived after this tick's event drain.
            for _ in erx.try_iter() {}
            match reply {
                Reply::ScanProgress(..) => unreachable!(),
                Reply::Scan(c) => {
                    v.stage.set_text("Select files, then preview their outputs");
                    v.progress.set_fraction(0.);
                    let mut s = v.state.borrow_mut();
                    s.candidates = c;
                    s.batch = None;
                    s.ready = false;
                    drop(s);
                    refresh_rows(&v);
                    rebuild_types(&v);
                    let note = if v.state.borrow().settings.workflow == Workflow::Convert {
                        " Resolve cache, proxy, gallery, and project-backup subfolders were excluded."
                    } else {
                        ""
                    };
                    v.banner.set_text(&format!("Sort by clicking a column heading. Filter by type or date, then use Select shown only. * marks a modification-date fallback.{note}"));
                }
                Reply::Plan(b) => {
                    v.stage
                        .set_text("Output preview ready · use Details to inspect each path");
                    v.progress.set_fraction(0.);
                    let staging = if b.staging_bytes() > 0 {
                        format!(
                            " · Temporary local copies up to {}",
                            human_bytes(b.staging_bytes())
                        )
                    } else {
                        String::new()
                    };
                    if b.settings.workflow == Workflow::Convert {
                        v.banner.set_text(&format!("Convert {} video{} · Estimated output {} · Source files stay unchanged. Output filenames are shown below.", b.items.len(), if b.items.len() == 1 { "" } else { "s" }, human_bytes(b.editing_bytes)));
                    } else {
                        v.banner.set_text(&format!("{} files to archive · Archive {} · Editing estimate {}{staging}. Details shows each output path.", b.items.len(), human_bytes(b.archive_bytes), human_bytes(b.editing_bytes)));
                    }
                    let mut s = v.state.borrow_mut();
                    s.batch = Some(*b);
                    s.ready = true;
                    drop(s);
                    refresh_rows(&v);
                }
                Reply::Finished(b) => {
                    v.banner
                        .set_text(&format!("{} · {}", b.status, b.summary()));
                    v.stage.set_text(&b.status);
                    v.progress.set_fraction(b.verified_fraction());
                    let mut s = v.state.borrow_mut();
                    s.batch = Some(*b);
                    s.ready = false;
                    drop(s);
                    refresh_rows(&v);
                }
                Reply::Settings(settings) => {
                    settings_feedback(
                        &v,
                        "Location saved. Return to the main screen to scan and review your ingest.",
                    );
                    let mut s = v.state.borrow_mut();
                    if s.settings.source != settings.source {
                        s.candidates.clear();
                    }
                    s.settings = settings;
                    s.batch = None;
                    s.ready = false;
                    drop(s);
                    refresh_rows(&v);
                    v.banner
                        .set_text("Location saved. Scan and review before starting a new batch.");
                }
                Reply::History(batches) => {
                    if !batches.is_empty() {
                        history_dialog(&v, batches);
                    } else {
                        v.banner.set_text("No previous batches yet.");
                    }
                }
                Reply::Probe(index, c) => {
                    v.state.borrow_mut().candidates[index] = c;
                    v.state.borrow_mut().batch = None;
                    refresh_rows(&v);
                }
                Reply::Error(e) => {
                    settings_feedback(&v, &e);
                    v.banner.set_text(&e);
                    v.stage.set_text("Needs attention · see the message above");
                }
                Reply::Info(info) => v.banner.set_text(&info),
            }
            refresh(&v);
            if v.state.borrow().closing {
                v.window.close();
            }
        }
        glib::ControlFlow::Continue
    });
    if demo {
        demo_rows(&view);
    } else if let Some(e) = error {
        view.banner.set_text(&e);
    }
    refresh(&view);
    window.present();
}
fn refresh_activity(v: &View) {
    let s = v.state.borrow();
    if !s.busy || s.cancel.requested() {
        return;
    }
    let lines: Vec<_> = s
        .activities
        .iter()
        .map(|(lane, p)| {
            let detail = if p.total == 0 {
                String::new()
            } else if p.stage.contains("Converting") || p.stage.contains("full decode") {
                format!(
                    " · {:.0}s / {:.0}s",
                    p.bytes as f64 / 1e6,
                    p.total as f64 / 1e6
                )
            } else {
                let elapsed = p.began.elapsed().as_secs_f64();
                let speed = if elapsed > 1. {
                    format!(
                        " · {}/s",
                        human_bytes((p.bytes.saturating_sub(p.first) as f64 / elapsed) as u64)
                    )
                } else {
                    String::new()
                };
                format!(
                    " · {} / {}{speed}",
                    human_bytes(p.bytes),
                    human_bytes(p.total)
                )
            };
            format!(
                "{} · File {} · {}{detail}",
                lane.label(),
                p.item + 1,
                p.stage
            )
        })
        .collect();
    if !lines.is_empty() {
        v.stage.set_text(&lines.join("\n"));
    } else {
        v.stage.set_text("Finishing queued work…");
    }
    if let Some(batch) = &s.batch {
        v.progress.set_fraction(batch.verified_fraction());
    }
}
fn location(title: &str) -> (gtk::Box, gtk::Label) {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    row.add_css_class("location");
    let name = label(title, "muted");
    name.set_width_chars(18);
    let path = label("Not configured", "");
    path.set_selectable(true);
    ellipsize(&path);
    row.append(&name);
    row.append(&path);
    (row, path)
}
fn send(v: &View, command: Command, stage: &str) {
    if v.state.borrow().busy {
        return;
    }
    settings_feedback(v, stage);
    v.state
        .borrow()
        .cancel
        .0
        .store(false, std::sync::atomic::Ordering::SeqCst);
    match v.tx.send(command) {
        Ok(()) => {
            v.state.borrow_mut().busy = true;
            v.state.borrow_mut().activities.clear();
            v.stage.set_text(stage);
            v.cancel.set_sensitive(true);
            refresh(v);
        }
        Err(_) => v
            .banner
            .set_text("Worker unavailable. Close other instances and restart the application."),
    }
}
fn refresh(v: &View) {
    let s = v.state.borrow();
    let settings = s.batch.as_ref().map(|b| &b.settings).unwrap_or(&s.settings);
    v.source
        .set_text(if settings.source.as_os_str().is_empty() {
            "Not configured"
        } else {
            settings.source.to_str().unwrap_or("Non-UTF8 source")
        });
    let converting = settings.workflow == Workflow::Convert;
    v.archive.set_text(if converting {
        "Not used for conversion"
    } else {
        settings
            .archive
            .as_ref()
            .and_then(|d| d.path.to_str())
            .unwrap_or("Not configured")
    });
    v.editing.set_text(
        if converting && s.batch.is_none() && v.placement.selected() == 0 {
            "Alongside each source · filename + .mov"
        } else {
            settings
                .editing
                .as_ref()
                .and_then(|d| d.path.to_str())
                .unwrap_or("Not configured")
        },
    );
    for l in [&v.archive, &v.editing] {
        l.set_tooltip_text(Some(l.text().as_str()));
    }
    v.mode
        .set_selected(u32::from(settings.workflow == Workflow::Convert));
    v.mode.set_sensitive(!s.busy);
    v.browse.set_sensitive(!s.busy);
    v.placement.set_visible(converting);
    v.placement.set_sensitive(!s.busy);
    v.bulk_actions.set_sensitive(!s.busy && s.batch.is_none());
    if let Some(column) = v.output_column.borrow().upgrade() {
        column.set_visible(s.batch.is_some());
    }
    v.source.set_tooltip_text(Some(v.source.text().as_str()));
    refresh_settings(v, &s.settings, s.busy);
    v.output_actions.set_visible(s.batch.is_some());
    v.output_actions.set_sensitive(!s.busy);
    v.scan
        .set_sensitive(!s.busy && (!s.settings.source.as_os_str().is_empty() || s.demo));
    v.cancel.set_visible(s.busy);
    let selected = s.candidates.iter().filter(|c| c.selected).count();
    v.start
        .set_sensitive(!s.busy && (selected > 0 || s.batch.is_some() || s.demo));
    v.start.set_label(if s.ready {
        if settings.workflow == Workflow::Convert {
            "Convert files"
        } else {
            "Archive & convert"
        }
    } else if s.batch.is_some() {
        "Recheck / Retry"
    } else {
        "Preview output files"
    });
    if s.batch.is_none() {
        v.summary.set_text(&format!(
            "{} shown · {selected} selected · {} scanned",
            v.store.n_items(),
            s.candidates.len()
        ));
        if selected == 0 && !s.busy {
            v.stage
                .set_text(if s.settings.source.as_os_str().is_empty() {
                    "Choose a source folder to enable scanning"
                } else {
                    "Scan the folder and select files to process"
                });
        }
    } else if let Some(batch) = &s.batch {
        v.summary.set_text(&batch.summary());
    }
    if s.settings.high_contrast {
        v.window.add_css_class("high-contrast");
    } else {
        v.window.remove_css_class("high-contrast");
    }
}
fn visible_candidate(s: &State, c: &Candidate) -> bool {
    (s.file_type.is_empty() || crate::catalog::file_type(c) == s.file_type)
        && s.since
            .is_none_or(|since| crate::catalog::recorded(c).date.date() >= since)
}
fn rebuild_types(v: &View) {
    let types: std::collections::BTreeSet<_> = {
        let s = v.state.borrow();
        if let Some(b) = &s.batch {
            b.items
                .iter()
                .map(|i| crate::catalog::file_type(&i.source))
                .collect()
        } else {
            s.candidates.iter().map(crate::catalog::file_type).collect()
        }
    };
    let mut names = vec!["All types".to_string()];
    names.extend(types);
    let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());
    v.type_filter.set_model(Some(&model));
    v.type_filter.set_selected(0);
}
fn refresh_rows(v: &View) {
    v.store.remove_all();
    let s = v.state.borrow();
    if let Some(b) = &s.batch {
        for (index, i) in b.items.iter().enumerate() {
            if !visible_candidate(&s, &i.source) {
                continue;
            }
            if s.filter == 1 && i.error.is_none() {
                continue;
            }
            if s.filter == 2 && !i.complete() {
                continue;
            }
            v.store.append(&glib::BoxedAnyObject::new(Row {
                index,
                candidate: i.source.clone(),
                item: Some(i.clone()),
            }));
        }
    } else {
        for (index, c) in s.candidates.iter().enumerate() {
            if !visible_candidate(&s, c) {
                continue;
            }
            if s.filter == 2 || (s.filter == 1 && c.probe_error.is_none()) {
                continue;
            }
            v.store.append(&glib::BoxedAnyObject::new(Row {
                index,
                candidate: c.clone(),
                item: None,
            }));
        }
    }
    drop(s);
    refresh(v);
}
fn update_row(v: &View, index: usize) {
    let s = v.state.borrow();
    let Some(b) = &s.batch else {
        return;
    };
    let Some(item) = b.items.get(index) else {
        return;
    };
    let include = visible_candidate(&s, &item.source)
        && (s.filter == 0
            || (s.filter == 1 && item.error.is_some())
            || (s.filter == 2 && item.complete()));
    let mut insertion = v.store.n_items();
    for pos in 0..v.store.n_items() {
        let object = v
            .store
            .item(pos)
            .unwrap()
            .downcast::<glib::BoxedAnyObject>()
            .unwrap();
        if object.borrow::<Row>().index == index {
            if !include {
                v.store.remove(pos);
                return;
            }
            object.borrow_mut::<Row>().item = Some(item.clone());
            if let Some(label) = v.status_labels.borrow().get(&index) {
                render_status(label, item);
            }
            return;
        }
        if object.borrow::<Row>().index > index {
            insertion = insertion.min(pos);
        }
    }
    if include {
        v.store.insert(
            insertion,
            &glib::BoxedAnyObject::new(Row {
                index,
                candidate: item.source.clone(),
                item: Some(item.clone()),
            }),
        );
    }
}
fn render_status(status: &gtk::Label, item: &Item) {
    for class in ["success", "error", "warning"] {
        status.remove_css_class(class);
    }
    let text = if item.archive == Stage::NotRequested {
        format!("Conversion: {}", item.editing)
    } else {
        format!("Archive: {} · Editing: {}", item.archive, item.editing)
    };
    status.set_text(&text);
    status.set_tooltip_text(Some(item.error.as_deref().unwrap_or(&text)));
    status.add_css_class(if item.complete() {
        "success"
    } else if item.error.is_some() {
        "error"
    } else {
        "warning"
    });
}
fn choose_folder(v: &View, which: u8) {
    if v.state.borrow().busy {
        return;
    }
    let dialog = gtk::FileDialog::builder()
        .title(match which {
            0 => "Select source folder",
            1 => "Select OS-mounted CIFS archive",
            _ => "Select editing destination",
        })
        .build();
    let v = v.clone();
    let window: gtk::Window = v
        .settings_window
        .borrow()
        .upgrade()
        .map(|w| w.upcast())
        .unwrap_or_else(|| v.window.clone().upcast());
    dialog.select_folder(Some(&window), gio::Cancellable::NONE, move |result| {
        if let Ok(file) = result {
            if let Some(path) = file.path() {
                if v.state.borrow().demo {
                    settings_feedback(
                        &v,
                        &format!(
                            "Prototype selection: {}. Demo does not save preferences.",
                            path.display()
                        ),
                    );
                    v.banner.set_text(&format!(
                        "Prototype selection: {}. Demo does not save settings or write media.",
                        path.display()
                    ));
                    return;
                }
                let s = v.state.borrow().settings.clone();
                send(
                    &v,
                    Command::Select(which, path, s),
                    "Validating selected location…",
                );
            } else {
                settings_feedback(
                    &v,
                    "Select an OS-mounted folder. smb:// and GVfs locations are unsupported.",
                );
                v.banner.set_text(
                    "Select an OS-mounted folder. smb:// and GVfs locations are unsupported.",
                );
            }
        }
    });
}
fn details_dialog(v: &View, row: Row) {
    let text = if let Some(i) = &row.item {
        serde_json::to_string_pretty(i).unwrap_or_default()
    } else {
        serde_json::to_string_pretty(&row.candidate).unwrap_or_default()
    };
    let dialog = adw::MessageDialog::new(
        Some(&v.window),
        Some(&row.candidate.relative.to_string_lossy()),
        Some(if row.item.is_some() {
            "Verification evidence, planned relative paths, and attempt diagnostics."
        } else if v.state.borrow().settings.workflow == Workflow::Convert {
            "Source metadata and conversion diagnostics. Converted MOV files keep the original video and use PCM audio."
        } else {
            "Choose whether this original also needs an editing copy. Sidecars are archived independently."
        }),
    );
    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let copy = action_button("Copy diagnostics");
    let copied = text.clone();
    copy.connect_clicked(move |b| b.clipboard().set_text(&copied));
    box_.append(&copy);
    if let Some(batch) = &v.state.borrow().batch
        && let Some(item) = &row.item
    {
        let paths = format!(
            "Original: {}\nOutput: {}",
            batch
                .settings
                .archive
                .as_ref()
                .map(|d| d.path.join(&item.archive_path))
                .unwrap_or_else(|| batch.settings.source.join(&item.source.relative))
                .display(),
            item.editing_path
                .as_ref()
                .and_then(|p| batch.settings.editing.as_ref().map(|e| e
                    .path
                    .join(p)
                    .display()
                    .to_string()))
                .unwrap_or_else(|| "Not requested".into())
        );
        let l = label(&paths, "muted");
        l.set_wrap(true);
        l.set_selectable(true);
        box_.append(&l);
    }
    let diagnostics = gtk::TextView::new();
    diagnostics.set_editable(false);
    diagnostics.set_monospace(true);
    diagnostics.set_wrap_mode(gtk::WrapMode::WordChar);
    diagnostics.buffer().set_text(&text);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&diagnostics)
        .min_content_height(220)
        .max_content_height(350)
        .build();
    box_.append(&scroll);
    dialog.set_extra_child(Some(&box_));
    dialog.add_response("close", "Close");
    if !v.state.borrow().busy {
        if row.item.is_none() {
            if v.state.borrow().settings.workflow == Workflow::Ingest {
                dialog.add_response("archive", "Archive only");
            }
            dialog.add_response("probe", "Retry probe");
        } else if !v.state.borrow().ready {
            dialog.add_response("retry", "Retry this file");
            if row
                .item
                .as_ref()
                .is_some_and(|i| i.archive == Stage::Verified && i.editing == Stage::NotRequested)
            {
                dialog.add_response("prepare", "Probe archived video");
            }
        }
    }
    let v = v.clone();
    dialog.connect_response(None, move |_, response| match response {
        "archive" => {
            if let Some(c) = v.state.borrow_mut().candidates.get_mut(row.index) {
                c.editing = false;
                c.selected = true;
            }
            refresh_rows(&v);
        }
        "probe" => {
            let s = v.state.borrow().settings.clone();
            send(
                &v,
                Command::Probe(row.index, s, row.candidate.clone()),
                "Probing selected file…",
            );
        }
        "retry" => {
            let id = v.state.borrow().batch.as_ref().unwrap().id.clone();
            send(
                &v,
                Command::Retry(id, Some(row.index)),
                "Retrying selected file…",
            );
        }
        "prepare" => {
            let id = v.state.borrow().batch.as_ref().unwrap().id.clone();
            send(
                &v,
                Command::PrepareArchivedEditing(id, row.index),
                "Probing the verified original…",
            );
        }
        _ => (),
    });
    dialog.present();
}
fn action_button(title: &str) -> gtk::Button {
    let button = gtk::Button::with_label(title);
    button.add_css_class("ingest-action");
    button
}
fn refresh_settings(v: &View, settings: &Settings, busy: bool) {
    if let Some(group) = v.settings_appearance.borrow().upgrade() {
        group.set_sensitive(!busy);
    }
    if let Some(window) = v.settings_window.borrow().upgrade() {
        if settings.high_contrast {
            window.add_css_class("high-contrast");
        } else {
            window.remove_css_class("high-contrast");
        }
    }
    let paths = [
        if settings.source.as_os_str().is_empty() {
            "Not configured".into()
        } else {
            settings.source.display().to_string()
        },
        settings
            .archive
            .as_ref()
            .map(|d| d.path.display().to_string())
            .unwrap_or_else(|| "Not configured".into()),
        settings
            .editing
            .as_ref()
            .map(|d| d.path.display().to_string())
            .unwrap_or_else(|| "Not configured".into()),
    ];
    for (row, path) in v.settings_rows.borrow().iter().zip(paths) {
        if let Some(row) = row.upgrade() {
            row.set_subtitle(&path);
            row.set_tooltip_text(Some(&path));
            row.set_sensitive(!busy);
        }
    }
}
fn settings_feedback(v: &View, message: &str) {
    if let Some(label) = v.settings_feedback.borrow().upgrade() {
        label.set_text(message);
    }
}
fn save_appearance(v: &View) {
    let s = v.state.borrow();
    let css = gtk::CssProvider::new();
    css.load_from_string(&format!(
        "window {{ font-size: {}px; }}",
        s.settings.text_scale * 15.
    ));
    gtk::style_context_add_provider_for_display(
        &gtk::prelude::WidgetExt::display(&v.window),
        &css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let result = if s.demo { Ok(()) } else { s.settings.save() };
    drop(s);
    match result {
        Ok(()) => settings_feedback(v, "Appearance updated."),
        Err(e) => settings_feedback(v, &format!("Could not save preferences: {e:#}")),
    }
    refresh(v);
}
fn settings_dialog(v: &View) {
    if let Some(window) = v.settings_window.borrow().upgrade() {
        window.present();
        return;
    }
    let window = adw::PreferencesWindow::builder()
        .transient_for(&v.window)
        .modal(true)
        .title("Settings")
        .default_width(680)
        .default_height(760)
        .search_enabled(false)
        .build();
    *v.settings_window.borrow_mut() = window.downgrade();
    let page = adw::PreferencesPage::new();
    page.set_title("Ingest settings");
    let locations = adw::PreferencesGroup::new();
    locations.set_title("Locations");
    locations.set_description(Some("Remembered for repeat ingests. Folder choices save automatically and apply to the next ingest."));
    let mut rows = Vec::new();
    for (title, which) in [
        ("Source folder", 0),
        ("Original archive", 1),
        ("Editing destination", 2),
    ] {
        let row = adw::ActionRow::new();
        row.set_title(title);
        row.set_use_markup(false);
        row.set_subtitle_lines(2);
        row.set_subtitle_selectable(true);
        let choose = action_button("Choose folder…");
        choose.set_valign(gtk::Align::Center);
        choose.update_property(&[gtk::accessible::Property::Label(&format!(
            "Choose {title} folder"
        ))]);
        row.add_suffix(&choose);
        row.set_activatable_widget(Some(&choose));
        let v = v.clone();
        choose.connect_clicked(move |_| choose_folder(&v, which));
        rows.push(row.downgrade());
        locations.add(&row);
    }
    *v.settings_rows.borrow_mut() = rows;
    page.add(&locations);
    let feedback_group = adw::PreferencesGroup::new();
    let feedback = label(
        if v.state.borrow().busy {
            "Settings can be changed after the current worker stops."
        } else {
            "Preferences save automatically. Ingest starts only from the main screen."
        },
        "muted",
    );
    feedback.set_wrap(true);
    feedback.set_selectable(true);
    *v.settings_feedback.borrow_mut() = feedback.downgrade();
    feedback_group.add(&feedback);
    page.add(&feedback_group);
    let storage_help = adw::PreferencesGroup::new();
    storage_help.set_description(Some("Originals need an OS-mounted CIFS share. Mount and authenticate the NAS through Linux first. Editing copies can use local storage or CIFS."));
    page.add(&storage_help);
    let conversion = adw::PreferencesGroup::new();
    conversion.set_title("Editing copies");
    for (title, subtitle) in [
        (
            "Processing",
            "Local copies and archiving overlap. Convert batches run up to two FFmpeg jobs at once; editing files publish after verification.",
        ),
        ("Video", "Keep the original codec without re-encoding"),
        (
            "Audio",
            "24-bit PCM at the original sample rate and channel layout",
        ),
        (
            "Verification",
            "Original checksum readback; editing stream checks, full decode, and checksum",
        ),
    ] {
        let row = adw::ActionRow::new();
        row.set_title(title);
        row.set_subtitle(subtitle);
        row.set_subtitle_lines(0);
        conversion.add(&row);
    }
    page.add(&conversion);
    let appearance = adw::PreferencesGroup::new();
    *v.settings_appearance.borrow_mut() = appearance.downgrade();
    appearance.set_title("Appearance");
    let contrast = adw::SwitchRow::new();
    contrast.set_title("High contrast");
    contrast.set_active(v.state.borrow().settings.high_contrast);
    let v2 = v.clone();
    contrast.connect_active_notify(move |row| {
        v2.state.borrow_mut().settings.high_contrast = row.is_active();
        save_appearance(&v2);
    });
    appearance.add(&contrast);
    let scale_row = adw::ActionRow::new();
    scale_row.set_title("Text size");
    let scale = gtk::DropDown::from_strings(&["100%", "150%", "200%"]);
    scale.set_valign(gtk::Align::Center);
    scale.update_property(&[gtk::accessible::Property::Label("Text size")]);
    scale.set_selected(((v.state.borrow().settings.text_scale - 1.) * 2.) as u32);
    let v2 = v.clone();
    scale.connect_selected_notify(move |scale| {
        v2.state.borrow_mut().settings.text_scale = 1. + f64::from(scale.selected()) * 0.5;
        save_appearance(&v2);
    });
    scale_row.add_suffix(&scale);
    appearance.add(&scale_row);
    page.add(&appearance);
    window.add(&page);
    refresh(v);
    window.present();
}
fn history_dialog(v: &View, batches: Vec<Batch>) {
    let dialog = adw::MessageDialog::new(
        Some(&v.window),
        Some("Batch history & recovery"),
        Some(
            "Recovery rechecks recorded files before reuse. Interrupted copies restart in a new owned partial. Select a batch to inspect or retry.",
        ),
    );
    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    for b in batches.into_iter().take(100) {
        let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let text = label(
            &format!(
                "{} · {}\n{}",
                chrono::DateTime::parse_from_rfc3339(&b.created_at)
                    .map(|d| d.format("%d %b %Y · %H:%M UTC").to_string())
                    .unwrap_or_else(|_| b.created_at.clone()),
                b.status,
                b.summary()
            ),
            "muted",
        );
        text.set_wrap(true);
        row.append(&text);
        let actions = gtk::FlowBox::new();
        actions.set_selection_mode(gtk::SelectionMode::None);
        actions.set_column_spacing(8);
        actions.set_max_children_per_line(4);
        let load = action_button("View / Recover");
        let report = action_button("Report");
        let inspect = action_button("Partials");
        let cleanup = action_button("Remove owned partials");
        for button in [&load, &report, &inspect, &cleanup] {
            actions.insert(button, -1);
        }
        row.append(&actions);
        let v2 = v.clone();
        let b2 = b.clone();
        let d = dialog.clone();
        load.connect_clicked(move |_| {
            let mut s = v2.state.borrow_mut();
            s.batch = Some(b2.clone());
            s.ready = false;
            drop(s);
            refresh_rows(&v2);
            v2.stage
                .set_text(&format!("{} · verification recorded earlier", b2.status));
            v2.banner.set_text(
                "Recovered journal. Press Recheck / Retry to validate files and resume this batch.",
            );
            d.close();
        });
        let v2 = v.clone();
        let id = b.id.clone();
        report.connect_clicked(move |_| {
            let dialog = gtk::FileDialog::builder()
                .title("Export batch report")
                .initial_name(format!("resolve-ingest-{id}.json"))
                .build();
            let v = v2.clone();
            let id = id.clone();
            dialog.save(Some(&v2.window), gio::Cancellable::NONE, move |result| {
                if let Ok(file) = result
                    && let Some(path) = file.path()
                {
                    send(&v, Command::Export(id, path), "Exporting report…");
                }
            });
        });
        let v2 = v.clone();
        let id = b.id.clone();
        let d = dialog.clone();
        inspect.connect_clicked(move |_| {
            d.close();
            send(
                &v2,
                Command::Cleanup(id.clone(), false),
                "Inspecting owned partials…",
            )
        });
        let v2 = v.clone();
        let id = b.id.clone();
        cleanup.connect_clicked(move |_| {
            send(
                &v2,
                Command::Cleanup(id.clone(), true),
                "Cleaning owned partials…",
            )
        });
        content.append(&row);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&content)
        .min_content_height(300)
        .max_content_height(500)
        .build();
    dialog.set_extra_child(Some(&scroll));
    dialog.add_response("close", "Close");
    dialog.present();
}
fn demo_rows(v: &View) {
    let mut s = v.state.borrow_mut();
    s.candidates = (0..1000)
        .map(|i| Candidate {
            relative: PathBuf::from(format!("DCIM/100MEDIA/DJI_20260904_{i:04}_D.MP4")),
            identity: Identity {
                dev: 0,
                ino: i,
                size: 148_000_000,
                mtime: 0,
                mtime_ns: 0,
                ctime: 0,
                ctime_ns: 0,
            },
            probe: None,
            probe_error: (i == 3).then(|| "Malformed media · retry probe or archive only".into()),
            selected: true,
            editing: i != 3,
            sidecar_for: None,
        })
        .collect();
    drop(s);
    rebuild_types(v);
    v.banner.set_text("Interactive prototype · 1,000 synthetic rows · no media writes. Preview output files simulates progress and partial failure.");
    refresh_rows(v);
}
fn demo_progress(v: &View) {
    v.stage.set_text("File 4 · Checking editing copy");
    v.summary
        .set_text("2 editing clips ready · 1 original archived · 1 needs attention");
    v.progress.set_fraction(0.65);
    v.banner.set_text("Archive unavailable. Reconnect the share, then retry. 3 originals are already verified. Prototype only.");
    v.start.set_label("Retry failed");
}
