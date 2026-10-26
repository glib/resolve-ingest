//! Preferences, history, clip details, and confirmations.
use super::{
    APP_ID, App, dispatch, location_subtitle, on, open_path,
    state::{Model, Msg, first_line},
    toast,
    worker::{Command, DestinationKind, Scope},
};
use crate::{
    catalog,
    diagnostics::human_bytes,
    model::{Batch, Stage},
    settings::{Verify, Workflow},
};
use adw::prelude::*;
use gtk::gio;
use std::{path::PathBuf, rc::Rc};

fn choose_folder(
    app: &Rc<App>,
    title: &str,
    initial: Option<PathBuf>,
    done: impl FnOnce(PathBuf) + 'static,
) {
    let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
    if let Some(initial) = initial.filter(|p| p.is_dir()) {
        dialog.set_initial_folder(Some(&gio::File::for_path(initial)));
    }
    let weak = Rc::downgrade(app);
    dialog.select_folder(
        Some(&app.ui().window),
        None::<&gio::Cancellable>,
        move |result| {
            let Ok(file) = result else {
                return;
            };
            match file.path() {
                Some(path) => done(path),
                None => {
                    if let Some(app) = weak.upgrade() {
                        toast(&app, "Choose a folder the operating system has mounted; network locations opened only in the file manager are not supported");
                    }
                }
            }
        },
    );
}

pub fn choose_source(app: &Rc<App>, workflow: Workflow) {
    let title = match workflow {
        Workflow::Ingest => "Choose a Card or Folder to Ingest",
        Workflow::Convert => "Choose a Folder to Convert",
    };
    let weak = Rc::downgrade(app);
    choose_folder(app, title, None, move |path| {
        if let Some(app) = weak.upgrade() {
            dispatch(&app, Msg::UseSource(path, workflow));
        }
    });
}

pub fn choose_destination(app: &Rc<App>, kind: DestinationKind) {
    let current = super::state::location_path(&app.model.borrow(), kind).map(|p| p.to_path_buf());
    let title = match kind {
        DestinationKind::Archive => "Choose the Archive Folder",
        DestinationKind::Editing => "Choose the Editing Copies Folder",
    };
    let weak = Rc::downgrade(app);
    choose_folder(app, title, current, move |path| {
        if let Some(app) = weak.upgrade() {
            dispatch(&app, Msg::SetDestination(kind, path));
        }
    });
}

/// Preferences rows that follow the model while the dialog is open.
pub struct PrefsRows {
    archive: adw::ActionRow,
    editing: adw::ActionRow,
    verify: adw::ComboRow,
    contrast: adw::SwitchRow,
    text: adw::ComboRow,
}
impl PrefsRows {
    pub fn render(&self, model: &Model) {
        let s = &model.settings;
        self.archive.set_subtitle(&location_subtitle(
            s.archive.as_ref().map(|d| d.path.as_path()),
            model.locations.archive.as_ref(),
        ));
        self.editing.set_subtitle(&location_subtitle(
            s.editing.as_ref().map(|d| d.path.as_path()),
            model.locations.editing.as_ref(),
        ));
        let busy = model.busy;
        self.archive.set_sensitive(!busy);
        self.editing.set_sensitive(!busy);
        self.verify
            .set_selected(u32::from(s.verify == Verify::Full));
        self.verify.set_subtitle(match s.verify {
            Verify::Fast => {
                "Checks streams, timing, and a checksum. Originals always get a full readback."
            }
            Verify::Full => {
                "Also decodes every frame, which takes longer. Originals always get a full readback."
            }
        });
        self.contrast.set_active(s.high_contrast);
        self.text
            .set_selected(((s.text_scale - 1.) * 2.).round() as u32);
    }
}

pub fn preferences(app: &Rc<App>) {
    let dialog = adw::PreferencesDialog::new();
    dialog.set_search_enabled(false);
    let page = adw::PreferencesPage::new();

    let locations = adw::PreferencesGroup::builder()
        .title("Locations")
        .description("Archives can be on a local disk, an external drive, or a CIFS/NFS share mounted by the system. Each location is remembered with its drive, so an unplugged drive is never mistaken for an empty folder.")
        .build();
    let archive = super::location_row(app, DestinationKind::Archive);
    let editing = super::location_row(app, DestinationKind::Editing);
    locations.add(&archive);
    locations.add(&editing);
    page.add(&locations);

    let copies = adw::PreferencesGroup::builder()
        .title("Editing Copies")
        .description("Video is copied without re-encoding. Audio becomes 24-bit PCM at the original sample rate and channel layout.")
        .build();
    let verify = adw::ComboRow::builder()
        .title("Check editing copies")
        .subtitle_lines(0)
        .model(&gtk::StringList::new(&["Fast", "Thorough"]))
        .build();
    verify.connect_selected_notify(on(app, |app, row: &adw::ComboRow| {
        let verify = if row.selected() == 1 {
            Verify::Full
        } else {
            Verify::Fast
        };
        if app.model.borrow().settings.verify != verify {
            dispatch(app, Msg::SetVerify(verify));
        }
    }));
    copies.add(&verify);
    page.add(&copies);

    let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
    let contrast = adw::SwitchRow::builder().title("High contrast").build();
    let text = adw::ComboRow::builder()
        .title("Text size")
        .model(&gtk::StringList::new(&["100%", "150%", "200%"]))
        .build();
    let appearance_changed = |app: &Rc<App>, high_contrast: bool, text_scale: f64| {
        let unchanged = {
            let s = &app.model.borrow().settings;
            s.high_contrast == high_contrast && s.text_scale == text_scale
        };
        if !unchanged {
            dispatch(
                app,
                Msg::SetAppearance {
                    high_contrast,
                    text_scale,
                },
            );
        }
    };
    {
        let text = text.clone();
        contrast.connect_active_notify(on(app, move |app, row: &adw::SwitchRow| {
            appearance_changed(app, row.is_active(), 1. + f64::from(text.selected()) * 0.5)
        }));
    }
    {
        let contrast = contrast.clone();
        text.connect_selected_notify(on(app, move |app, row: &adw::ComboRow| {
            appearance_changed(
                app,
                contrast.is_active(),
                1. + f64::from(row.selected()) * 0.5,
            )
        }));
    }
    appearance.add(&contrast);
    appearance.add(&text);
    page.add(&appearance);
    dialog.add(&page);

    let rows = PrefsRows {
        archive,
        editing,
        verify,
        contrast,
        text,
    };
    rows.render(&app.model.borrow());
    *app.ui().prefs.borrow_mut() = Some(rows);
    dialog.connect_closed(on(app, |app, _: &adw::PreferencesDialog| {
        app.ui().prefs.borrow_mut().take();
    }));
    dialog.present(Some(&app.ui().window));
}

fn sheet(title: &str, content: &impl IsA<gtk::Widget>, width: i32, height: i32) -> adw::Dialog {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&adw::HeaderBar::new());
    view.set_content(Some(content));
    adw::Dialog::builder()
        .title(title)
        .content_width(width)
        .content_height(height)
        .child(&view)
        .build()
}

fn local_time(rfc3339: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(rfc3339)
        .map(|d| {
            d.with_timezone(&chrono::Local)
                .format("%a %-d %b %Y · %H:%M")
                .to_string()
        })
        .unwrap_or_else(|_| rfc3339.to_string())
}

fn save_report(app: &Rc<App>, batch: String) {
    let dialog = gtk::FileDialog::builder()
        .title("Export Report")
        .initial_name(format!(
            "resolve-ingest-{}.json",
            &batch[..8.min(batch.len())]
        ))
        .build();
    let weak = Rc::downgrade(app);
    dialog.save(
        Some(&app.ui().window),
        None::<&gio::Cancellable>,
        move |result| {
            if let (Some(app), Ok(file)) = (weak.upgrade(), result)
                && let Some(path) = file.path()
            {
                app.worker.send(Command::Export { batch, path });
            }
        },
    );
}

pub fn history(app: &Rc<App>, batches: Vec<Batch>) {
    let content: gtk::Widget = if batches.is_empty() {
        adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No ingests yet")
            .description(
                "Finished and interrupted ingests appear here with their verification reports.",
            )
            .build()
            .upcast()
    } else {
        let list = super::boxed_list();
        let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
        page.set_margin_top(12);
        page.set_margin_bottom(24);
        page.set_margin_start(12);
        page.set_margin_end(12);
        page.append(&list);
        let scroll = gtk::ScrolledWindow::builder()
            .child(&adw::Clamp::builder().maximum_size(640).child(&page).build())
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .build();
        for batch in batches.iter().take(200) {
            list.append(&history_row(app, batch));
        }
        scroll.upcast()
    };
    let dialog = sheet("History", &content, 620, 560);
    dialog.present(Some(&app.ui().window));
}

fn history_row(app: &Rc<App>, batch: &Batch) -> adw::ActionRow {
    let complete = batch.status == "Completed";
    let source = batch
        .settings
        .source
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let kind = if batch.settings.workflow == Workflow::Convert {
        "Conversion"
    } else {
        "Ingest"
    };
    let row = adw::ActionRow::builder()
        .title(local_time(&batch.created_at))
        .subtitle(format!(
            "{kind} from {source} · {} {} · {}",
            batch.items.len(),
            if batch.items.len() == 1 {
                "file"
            } else {
                "files"
            },
            batch.status
        ))
        .activatable(true)
        .build();
    let icon = gtk::Image::from_icon_name(if complete {
        "object-select-symbolic"
    } else {
        "dialog-warning-symbolic"
    });
    icon.add_css_class(if complete { "success" } else { "warning" });
    row.add_prefix(&icon);
    let report = gtk::Button::from_icon_name("document-save-symbolic");
    report.set_tooltip_text(Some("Export Report"));
    let cleanup = gtk::Button::from_icon_name("edit-clear-all-symbolic");
    cleanup.set_tooltip_text(Some("Find Partial Files"));
    for b in [&report, &cleanup] {
        b.add_css_class("flat");
        b.set_valign(gtk::Align::Center);
        row.add_suffix(b);
    }
    let id = batch.id.clone();
    report.connect_clicked(on(app, move |app, _: &gtk::Button| {
        save_report(app, id.clone())
    }));
    let id = batch.id.clone();
    cleanup.connect_clicked(on(app, move |app, _: &gtk::Button| {
        app.worker.send(Command::Partials(id.clone()));
    }));
    let batch = batch.clone();
    row.connect_activated(on(app, move |app, row: &adw::ActionRow| {
        if let Some(dialog) = row
            .ancestor(adw::Dialog::static_type())
            .and_downcast::<adw::Dialog>()
        {
            dialog.close();
        }
        dispatch(app, Msg::OpenBatch(Box::new(batch.clone())));
    }));
    row
}

pub fn partials(app: &Rc<App>, batch: String, files: Vec<(bool, PathBuf, u64)>) {
    if files.is_empty() {
        toast(app, "No partial files left from this ingest");
        return;
    }
    let bytes: u64 = files.iter().map(|f| f.2).sum();
    let dialog = adw::AlertDialog::new(
        Some(&format!(
            "Remove {} partial {}?",
            files.len(),
            if files.len() == 1 { "file" } else { "files" }
        )),
        Some(&format!(
            "Interrupted copies left {} of incomplete files. Verified originals and editing copies are never removed.",
            human_bytes(bytes)
        )),
    );
    dialog.add_responses(&[("cancel", "Keep"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = Rc::downgrade(app);
    dialog.connect_response(None, move |_, response| {
        if response == "remove"
            && let Some(app) = weak.upgrade()
        {
            app.worker.send(Command::Cleanup(batch.clone()));
        }
    });
    dialog.present(Some(&app.ui().window));
}

pub fn confirm_close(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let dialog = adw::AlertDialog::new(
        Some("Stop the ingest?"),
        Some(
            "Work in progress stops safely and verified files are kept. Partial files stay marked incomplete so the ingest can be resumed from History.",
        ),
    );
    dialog.add_responses(&[("keep", "Keep Working"), ("stop", "Stop and Close")]);
    dialog.set_response_appearance("stop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("keep"));
    dialog.set_close_response("keep");
    let weak = Rc::downgrade(app);
    dialog.connect_response(None, move |_, response| {
        if response == "stop"
            && let Some(app) = weak.upgrade()
        {
            dispatch(&app, Msg::RequestClose);
        }
    });
    dialog.present(Some(window));
}

pub fn about(app: &Rc<App>) {
    adw::AboutDialog::builder()
        .application_name("Resolve Ingest")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Verified camera archives and PCM editing copies for DaVinci Resolve")
        .license_type(gtk::License::MitX11)
        .build()
        .present(Some(&app.ui().window));
}

fn property(title: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(value)
        .subtitle_selectable(true)
        .subtitle_lines(0)
        .build();
    row.add_css_class("property");
    row
}

fn group(title: &str) -> adw::PreferencesGroup {
    adw::PreferencesGroup::builder().title(title).build()
}

fn stage_text(stage: &Stage) -> String {
    match stage {
        Stage::NotRequested => "Not requested".into(),
        other => other.to_string(),
    }
}

/// Everything known about one clip: metadata, destinations, evidence, problems, and actions.
pub fn details(app: &Rc<App>, index: usize) {
    let model = app.model.borrow();
    let showing_batch = model.showing_batch();
    let batch = model.batch.as_ref().filter(|_| showing_batch);
    let item = batch.and_then(|b| b.items.get(index));
    let Some(candidate) = item
        .map(|i| &i.source)
        .or_else(|| model.candidates.get(index))
    else {
        return;
    };
    let name = candidate
        .relative
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let page = adw::PreferencesPage::new();

    let clip = group("Clip");
    let recorded = catalog::recorded(candidate);
    clip.add(&property(
        "Recorded",
        &format!(
            "{}\n{}",
            recorded.date.format("%A %-d %B %Y · %H:%M:%S"),
            recorded.origin
        ),
    ));
    clip.add(&property("Size", &human_bytes(candidate.identity.size)));
    if let Some(probe) = &candidate.probe {
        if probe.duration() > 0. {
            clip.add(&property(
                "Length",
                &super::clips::duration_text(probe.duration()),
            ));
        }
        if let Some(v) = probe.video() {
            let fps = v
                .avg_frame_rate
                .as_deref()
                .and_then(crate::media::rate)
                .map(|r| format!(" · {r:.2} fps"))
                .unwrap_or_default();
            clip.add(&property(
                "Video",
                &format!(
                    "{} · {}×{}{fps} · {}",
                    v.codec_name.to_uppercase(),
                    v.width.unwrap_or(0),
                    v.height.unwrap_or(0),
                    v.pix_fmt.as_deref().unwrap_or("unknown pixel format")
                ),
            ));
        }
        let audio: Vec<_> = probe
            .audio()
            .map(|a| {
                format!(
                    "{} · {} Hz · {} ch",
                    a.codec_name.to_uppercase(),
                    a.sample_rate.as_deref().unwrap_or("?"),
                    a.channels.unwrap_or(0)
                )
            })
            .collect();
        clip.add(&property(
            "Audio",
            &if audio.is_empty() {
                "No audio".into()
            } else {
                audio.join("\n")
            },
        ));
    }
    let source_root = batch
        .map(|b| b.settings.source.clone())
        .or_else(|| model.source.as_ref().map(|s| s.path.clone()))
        .unwrap_or_default();
    clip.add(&property(
        "Source",
        &source_root.join(&candidate.relative).display().to_string(),
    ));
    if let Some(archived) = &candidate.archived {
        clip.add(&property(
            "Already archived",
            &archived.display().to_string(),
        ));
    }
    page.add(&clip);

    let mut folders: Vec<(String, PathBuf)> = vec![];
    if let (Some(item), Some(batch)) = (item, batch) {
        if item.archive != Stage::NotRequested {
            let original = group("Original");
            original.add(&property("Status", &stage_text(&item.archive)));
            if let Some(archive) = &batch.settings.archive {
                let path = archive.path.join(&item.archive_path);
                original.add(&property("Location", &path.display().to_string()));
                if let Some(parent) = path.parent() {
                    folders.push(("Show Original".into(), parent.to_path_buf()));
                }
            }
            if let Some(e) = &item.archive_evidence {
                original.add(&property("SHA-256", &e.sha256));
                original.add(&property("Checks", &e.checks.join("\n")));
            }
            page.add(&original);
        }
        let editing = group(if batch.settings.workflow == Workflow::Convert {
            "Converted File"
        } else {
            "Editing Copy"
        });
        editing.add(&property("Status", &stage_text(&item.editing)));
        if let (Some(path), Some(dest)) = (&item.editing_path, &batch.settings.editing) {
            let path = dest.path.join(path);
            editing.add(&property("Location", &path.display().to_string()));
            if let Some(parent) = path.parent() {
                folders.push(("Show Editing Copy".into(), parent.to_path_buf()));
            }
        }
        if let Some(e) = &item.output_evidence {
            editing.add(&property("SHA-256", &e.sha256));
            editing.add(&property("Checks", &e.checks.join("\n")));
        }
        page.add(&editing);
    }

    let problems: Vec<String> = item
        .and_then(|i| i.error.clone())
        .or_else(|| candidate.probe_error.clone())
        .into_iter()
        .chain(item.map(|i| i.notices.clone()).unwrap_or_default())
        .collect();
    if !problems.is_empty() {
        let notes = group("Notes");
        for (i, text) in problems.iter().enumerate() {
            let row = property(
                &first_line(text),
                text.lines().skip(1).collect::<Vec<_>>().join("\n").trim(),
            );
            if i == 0
                && (item.is_some_and(|i| i.error.is_some()) || candidate.probe_error.is_some())
            {
                row.add_css_class("error");
            }
            notes.add(&row);
        }
        page.add(&notes);
    }

    // Actions available for this clip on the current screen.
    let mut actions: Vec<(&str, bool, Msg)> = vec![];
    if !model.busy {
        if let Some(item) = item {
            if !item.complete() {
                actions.push(("Retry This File", true, Msg::Retry(Scope::Item(index))));
            }
            if item.archive == Stage::Verified
                && item.editing == Stage::NotRequested
                && batch.is_some_and(|b| b.settings.workflow == Workflow::Ingest)
            {
                actions.push(("Create Editing Copy", false, Msg::PrepareEditing(index)));
            }
        } else if model.can_select() && candidate.probe_error.is_some() {
            actions.push(("Retry Reading", false, Msg::RetryProbe(index)));
            if model.workflow() == Workflow::Ingest && !candidate.selected {
                actions.push(("Archive Original Only", true, Msg::ArchiveOnly(index)));
            }
        }
    }
    let diagnostics = match item {
        Some(i) => serde_json::to_string_pretty(i),
        None => serde_json::to_string_pretty(candidate),
    }
    .unwrap_or_default();
    drop(model);

    let buttons = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(3)
        .column_spacing(8)
        .row_spacing(8)
        .halign(gtk::Align::Center)
        .build();
    let dialog = sheet(&name, &page, 600, 680);
    for (label, suggested, msg) in actions {
        let button = super::pill(label, suggested);
        let msg = std::cell::Cell::new(Some(msg));
        let d = dialog.downgrade();
        button.connect_clicked(on(app, move |app, _: &gtk::Button| {
            if let Some(msg) = msg.take() {
                if let Some(d) = d.upgrade() {
                    d.close();
                }
                dispatch(app, msg);
            }
        }));
        buttons.append(&button);
    }
    for (label, folder) in folders {
        let button = super::pill(&label, false);
        button.connect_clicked(on(app, move |app, _: &gtk::Button| {
            open_path(app, Some(folder.clone()))
        }));
        buttons.append(&button);
    }
    let copy = super::pill("Copy Diagnostics", false);
    copy.connect_clicked(on(app, move |app, b: &gtk::Button| {
        b.clipboard().set_text(&diagnostics);
        toast(app, "Diagnostics copied");
    }));
    buttons.append(&copy);
    let footer = adw::PreferencesGroup::new();
    footer.add(&buttons);
    page.add(&footer);
    dialog.present(Some(&app.ui().window));
}
