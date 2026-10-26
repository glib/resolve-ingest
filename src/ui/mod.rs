//! GTK4/libadwaita desktop. `state` decides; this module draws the model and performs effects.
mod cards;
mod clips;
mod demo;
mod dialogs;
mod state;
mod worker;

use crate::{
    diagnostics::human_bytes,
    settings::{Destination, Settings, Workflow},
};
use adw::prelude::*;
use gtk::{gio, glib};
use state::{Card, Effect, Model, Msg, Placement, Preflight, Screen};
use std::{
    cell::{Cell, OnceCell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    time::{Duration, Instant},
};
use worker::{DestinationKind, LocationInfo, Scope, Worker};

const APP_ID: &str = "io.github.resolve_ingest.ResolveIngest";

pub(crate) struct App {
    model: RefCell<Model>,
    worker: Worker,
    demo: bool,
    ui: OnceCell<Ui>,
    preflight_timer: RefCell<Option<glib::SourceId>>,
    text_css: gtk::CssProvider,
    monitor: RefCell<Option<gio::VolumeMonitor>>,
    allow_close: Cell<bool>,
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    title: adw::WindowTitle,
    banner: adw::Banner,
    stack: gtk::Stack,
    setup_rows: [adw::ActionRow; 2],
    cards: gtk::ListBox,
    shown_cards: RefCell<Vec<Card>>,
    waiting_chips: [gtk::Button; 2],
    scan_title: gtk::Label,
    scan_detail: gtk::Label,
    headline: gtk::Label,
    subline: gtk::Label,
    chips: [gtk::Button; 2],
    progress: gtk::ProgressBar,
    progress_box: gtk::Box,
    progress_text: gtk::Label,
    activity: gtk::Label,
    list_tools: gtk::Box,
    list_stack: gtk::Stack,
    empty: adw::StatusPage,
    show_archived: gtk::Button,
    clips: clips::Clips,
    bar_title: gtk::Label,
    bar_detail: gtk::Label,
    placement: gtk::DropDown,
    start: gtk::Button,
    cancel: gtk::Button,
    eject: gtk::Button,
    retry: gtk::Button,
    open_editing: gtk::Button,
    new_session: gtk::Button,
    view: adw::ToolbarView,
    prefs: RefCell<Option<dialogs::PrefsRows>>,
}

impl App {
    fn ui(&self) -> &Ui {
        self.ui.get().expect("window built")
    }
}

/// Starts the desktop app, optionally opening `folder` as soon as the window is ready.
pub fn run(demo: bool, folder: Option<PathBuf>) {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| {
        adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
        let css = gtk::CssProvider::new();
        css.load_from_string(include_str!("style.css"));
        gtk::style_context_add_provider_for_display(
            &gtk::gdk::Display::default().unwrap(),
            &css,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.connect_activate(move |application| {
        if let Some(window) = application.active_window() {
            window.present();
            return;
        }
        build(application, demo, folder.clone());
    });
    app.run_with_args(&["resolve-ingest"]);
}

fn demo_settings() -> Settings {
    let destination = |path: &str| Destination {
        path: path.into(),
        expected_mount: String::new(),
        shared: false,
    };
    Settings {
        archive: Some(destination("/mnt/nas/Footage Archive")),
        editing: Some(destination("/home/me/Videos/Resolve Media")),
        ..Default::default()
    }
}

fn build(application: &adw::Application, demo: bool, folder: Option<PathBuf>) {
    let (settings, load_error) = if demo {
        (demo_settings(), None)
    } else {
        match Settings::load() {
            Ok(s) => (s, None),
            Err(e) => (
                Settings::default(),
                Some(format!("Settings could not be loaded: {e:#}")),
            ),
        }
    };
    let app = Rc::new(App {
        model: RefCell::new(Model::new(settings)),
        worker: Worker::spawn(demo),
        demo,
        ui: OnceCell::new(),
        preflight_timer: RefCell::new(None),
        text_css: gtk::CssProvider::new(),
        monitor: RefCell::new(None),
        allow_close: Cell::new(false),
    });
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &app.text_css,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let ui = build_ui(application, &app);
    let _ = app.ui.set(ui);
    install_actions(application, &app);
    apply_appearance(&app);
    if let Some(error) = load_error {
        app.model.borrow_mut().problem = Some(error);
    }

    if demo {
        // A synthetic card arrives shortly after launch, as a real one would.
        let weak = Rc::downgrade(&app);
        glib::timeout_add_local_once(Duration::from_millis(900), move || {
            if let Some(app) = weak.upgrade() {
                dispatch(
                    &app,
                    Msg::CardsChanged(vec![Card {
                        name: "DJI Action 4".into(),
                        path: "/run/media/demo/DJI_ACTION4".into(),
                        can_eject: true,
                    }]),
                );
            }
        });
    } else {
        let weak = Rc::downgrade(&app);
        let monitor = cards::watch(move |cards| {
            if let Some(app) = weak.upgrade() {
                dispatch(&app, Msg::CardsChanged(cards));
            }
        });
        *app.monitor.borrow_mut() = Some(monitor);
    }
    // A file opens its folder, e.g. from a file manager's "Open With".
    let folder = folder.map(|p| match p.parent() {
        Some(parent) if p.is_file() => parent.to_path_buf(),
        _ => p,
    });
    if let Some(folder) = folder {
        let weak = Rc::downgrade(&app);
        // After the first card notification, so a card folder is recognized as one.
        glib::idle_add_local_once(move || {
            if let Some(app) = weak.upgrade() {
                let workflow = if cards::is_card(&folder) {
                    Workflow::Ingest
                } else {
                    Workflow::Convert
                };
                dispatch(&app, Msg::UseSource(folder, workflow));
            }
        });
    }

    let weak = Rc::downgrade(&app);
    glib::timeout_add_local(Duration::from_millis(100), move || {
        let Some(app) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        pump(&app);
        glib::ControlFlow::Continue
    });
    render(&app);
    // Widgets hold the app weakly; the window owns it until it is destroyed.
    let owner = RefCell::new(Some(app.clone()));
    app.ui().window.connect_destroy(move |_| {
        owner.borrow_mut().take();
    });
    app.ui().window.present();
}

/// Drains the worker. Progress arrives in bursts, so rows are refreshed at most once a tick.
fn pump(app: &Rc<App>) {
    let (replies, events) = app.worker.drain();
    let running = app.model.borrow().screen == Screen::Running;
    if !events.is_empty() {
        dispatch(app, Msg::Events(events, Instant::now()));
    } else if running {
        render(app);
    }
    for reply in replies {
        dispatch(app, Msg::Reply(reply));
    }
}

pub(crate) fn dispatch(app: &Rc<App>, msg: Msg) {
    let effects = app.model.borrow_mut().update(msg);
    let mut rows = Vec::new();
    for effect in effects {
        match effect {
            Effect::RefreshRow(i) => rows.push(i),
            other => perform(app, other),
        }
    }
    if !rows.is_empty() {
        let model = app.model.borrow();
        for i in rows {
            app.ui().clips.refresh(&model, i);
        }
    }
    render(app);
}

fn perform(app: &Rc<App>, effect: Effect) {
    let ui = app.ui();
    match effect {
        Effect::Send(command) => {
            if !app.worker.send(command) {
                toast(
                    app,
                    "The background worker stopped. Restart the application.",
                );
            }
        }
        Effect::CancelWorker => app.worker.cancel(),
        Effect::SchedulePreflight => {
            if let Some(id) = app.preflight_timer.borrow_mut().take() {
                id.remove();
            }
            let weak = Rc::downgrade(app);
            let id = glib::timeout_add_local_once(Duration::from_millis(350), move || {
                if let Some(app) = weak.upgrade() {
                    app.preflight_timer.borrow_mut().take();
                    dispatch(&app, Msg::RunPreflight);
                }
            });
            *app.preflight_timer.borrow_mut() = Some(id);
        }
        Effect::RebuildRows => ui.clips.rebuild(&app.model.borrow()),
        Effect::RefreshRows => ui.clips.refresh_all(&app.model.borrow()),
        Effect::RefreshRow(i) => ui.clips.refresh(&app.model.borrow(), i),
        Effect::SaveSettings => {
            if !app.demo
                && let Err(e) = app.model.borrow().settings.save()
            {
                toast(app, &format!("Could not save preferences: {e:#}"));
            }
        }
        Effect::ApplyAppearance => apply_appearance(app),
        Effect::Toast(text) => toast(app, &text),
        Effect::Notify { title, body } => {
            if !ui.window.is_active()
                && let Some(application) = ui.window.application()
            {
                let notification = gio::Notification::new(&title);
                notification.set_body(Some(&body));
                application.send_notification(Some("batch-finished"), &notification);
            }
        }
        Effect::ShowHistory(batches) => dialogs::history(app, batches),
        Effect::ShowPartials { batch, files } => dialogs::partials(app, batch, files),
        Effect::Close => {
            app.allow_close.set(true);
            ui.window.close();
        }
    }
}

pub(crate) fn toast(app: &App, text: &str) {
    let toast = adw::Toast::new(text);
    toast.set_timeout(4);
    app.ui().toasts.add_toast(toast);
}

fn apply_appearance(app: &App) {
    let model = app.model.borrow();
    app.text_css.load_from_string(&format!(
        "window {{ font-size: {}px; }}",
        model.settings.text_scale * 15.
    ));
    if let Some(ui) = app.ui.get() {
        if model.settings.high_contrast {
            ui.window.add_css_class("high-contrast");
        } else {
            ui.window.remove_css_class("high-contrast");
        }
    }
}

/// Wraps a handler so it holds the app weakly; widgets never keep the app alive.
fn on<T: ?Sized>(app: &Rc<App>, f: impl Fn(&Rc<App>, &T) + 'static) -> impl Fn(&T) + 'static {
    let weak = Rc::downgrade(app);
    move |arg| {
        if let Some(app) = weak.upgrade() {
            f(&app, arg);
        }
    }
}

fn pill(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::with_label(label);
    button.add_css_class("pill");
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn boxed_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.add_css_class("boxed-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    list
}

fn wrap_box() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(4)
        .column_spacing(6)
        .row_spacing(6)
        .build()
}

fn build_ui(application: &adw::Application, app: &Rc<App>) -> Ui {
    let weak = Rc::downgrade(app);
    let window = adw::ApplicationWindow::builder()
        .application(application)
        .title("Resolve Ingest")
        .default_width(1100)
        .default_height(760)
        .width_request(360)
        .height_request(480)
        .build();

    let header = adw::HeaderBar::new();
    let title = adw::WindowTitle::new("Resolve Ingest", "");
    header.set_title_widget(Some(&title));
    let menu = gio::Menu::new();
    let session = gio::Menu::new();
    session.append(Some("Choose Folder…"), Some("win.choose-folder"));
    session.append(Some("Convert Files…"), Some("win.convert"));
    session.append(Some("Rescan"), Some("win.rescan"));
    menu.append_section(None, &session);
    let general = gio::Menu::new();
    general.append(Some("History"), Some("win.history"));
    general.append(Some("Preferences"), Some("win.preferences"));
    general.append(Some("About Resolve Ingest"), Some("win.about"));
    menu.append_section(None, &general);
    let menu_button = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main Menu")
        .build();
    header.pack_end(&menu_button);

    let banner = adw::Banner::new("");
    banner.connect_button_clicked(on(app, |app, _: &adw::Banner| {
        let interrupted = {
            let mut model = app.model.borrow_mut();
            model.problem.take().is_none() && std::mem::take(&mut model.interrupted) > 0
        };
        if interrupted {
            dispatch(app, Msg::RequestHistory);
        } else {
            render(app);
        }
    }));

    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .vexpand(true)
        .build();

    // Setup: the two remembered destinations. The screen advances once both are set.
    let setup_rows = [
        location_row(app, DestinationKind::Archive),
        location_row(app, DestinationKind::Editing),
    ];
    let setup_list = boxed_list();
    for row in &setup_rows {
        setup_list.append(row);
    }
    let setup = status_page(
        "folder-download-symbolic",
        "Where should footage go?",
        "Originals are copied to the archive and checked byte for byte. Editing copies keep the camera's video and get 24-bit PCM audio for Resolve.",
        &clamp(&setup_list, 520),
    );
    stack.add_named(&setup, Some("setup"));

    // Waiting: no source yet.
    let cards = boxed_list();
    cards.set_visible(false);
    let choose = pill("Choose Folder…", false);
    choose.set_action_name(Some("win.choose-folder"));
    let convert = gtk::Button::with_label("Convert Existing Files…");
    convert.add_css_class("flat");
    convert.set_action_name(Some("win.convert"));
    let buttons = gtk::Box::new(gtk::Orientation::Vertical, 6);
    buttons.set_halign(gtk::Align::Center);
    buttons.append(&choose);
    buttons.append(&convert);
    let waiting_chips = [
        chip(app, DestinationKind::Archive),
        chip(app, DestinationKind::Editing),
    ];
    let waiting_locations = wrap_box();
    waiting_locations.set_halign(gtk::Align::Center);
    for c in &waiting_chips {
        waiting_locations.append(c);
    }
    let waiting_box = gtk::Box::new(gtk::Orientation::Vertical, 24);
    waiting_box.append(&cards);
    waiting_box.append(&buttons);
    waiting_box.append(&waiting_locations);
    let waiting = status_page(
        "media-flash-symbolic",
        "Insert a camera card",
        "Cards are found automatically. Clips that are already in the archive are left unselected.",
        &clamp(&waiting_box, 560),
    );
    stack.add_named(&waiting, Some("waiting"));

    // Scanning.
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .width_request(32)
        .height_request(32)
        .halign(gtk::Align::Center)
        .build();
    let scan_title = gtk::Label::new(None);
    scan_title.add_css_class("title-1");
    scan_title.set_wrap(true);
    scan_title.set_justify(gtk::Justification::Center);
    let scan_detail = gtk::Label::new(None);
    scan_detail.add_css_class("dim-label");
    scan_detail.add_css_class("numeric");
    scan_detail.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    scan_detail.set_max_width_chars(60);
    let scan_cancel = pill("Cancel", false);
    scan_cancel.set_halign(gtk::Align::Center);
    scan_cancel.connect_clicked(on(app, |app, _: &gtk::Button| dispatch(app, Msg::Cancel)));
    let scan_box = gtk::Box::new(gtk::Orientation::Vertical, 18);
    scan_box.set_valign(gtk::Align::Center);
    scan_box.set_margin_start(24);
    scan_box.set_margin_end(24);
    scan_box.append(&spinner);
    scan_box.append(&scan_title);
    scan_box.append(&scan_detail);
    scan_box.append(&scan_cancel);
    stack.add_named(&scan_box, Some("scanning"));

    // Session: review, running, and done share the clip table.
    let headline = gtk::Label::builder().xalign(0.).wrap(true).build();
    headline.add_css_class("title-2");
    let subline = gtk::Label::builder()
        .xalign(0.)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();
    subline.add_css_class("dim-label");
    let chips = [
        chip(app, DestinationKind::Archive),
        chip(app, DestinationKind::Editing),
    ];
    let chip_box = wrap_box();
    chip_box.set_halign(gtk::Align::Start);
    for c in &chips {
        chip_box.append(c);
    }
    let heading = gtk::Box::new(gtk::Orientation::Vertical, 4);
    heading.append(&headline);
    heading.append(&subline);
    heading.append(&chip_box);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("batch-progress");
    let progress_text = gtk::Label::builder()
        .xalign(0.)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    progress_text.add_css_class("numeric");
    let activity = gtk::Label::builder()
        .xalign(0.)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    activity.add_css_class("dim-label");
    activity.add_css_class("caption");
    let progress_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    progress_box.append(&progress);
    progress_box.append(&progress_text);
    progress_box.append(&activity);

    let select_new = gtk::Button::with_label("Select New");
    select_new.add_css_class("flat");
    select_new.connect_clicked(on(app, |app, _: &gtk::Button| {
        dispatch(app, Msg::SelectNew)
    }));
    let select_none = gtk::Button::with_label("Select None");
    select_none.add_css_class("flat");
    select_none.connect_clicked(on(app, |app, _: &gtk::Button| {
        dispatch(app, Msg::SelectNone)
    }));
    let filter_menu = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .tooltip_text("Show")
        .build();
    filter_menu.add_css_class("flat");
    {
        let weak = weak.clone();
        filter_menu.set_create_popup_func(move |button| {
            if let Some(app) = weak.upgrade() {
                button.set_popover(Some(&filter_popover(&app)));
            }
        });
    }
    let list_tools = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    list_tools.set_halign(gtk::Align::End);
    list_tools.append(&select_new);
    list_tools.append(&select_none);
    list_tools.append(&filter_menu);

    let clips = clips::Clips::new(&weak);
    let session = gtk::Box::new(gtk::Orientation::Vertical, 12);
    session.add_css_class("session");
    session.append(&heading);
    session.append(&progress_box);
    session.append(&list_tools);
    let show_archived = pill("Show Archived Clips", false);
    show_archived.set_halign(gtk::Align::Center);
    show_archived.connect_clicked(on(app, |app, _: &gtk::Button| {
        dispatch(app, Msg::ShowArchived(true))
    }));
    let empty = adw::StatusPage::builder()
        .icon_name("object-select-symbolic")
        .child(&show_archived)
        .build();
    empty.add_css_class("compact");
    let list_stack = gtk::Stack::new();
    list_stack.add_named(&clips.widget, Some("list"));
    list_stack.add_named(&empty, Some("empty"));
    session.append(&list_stack);
    stack.add_named(&session, Some("session"));

    // Bottom bar: one summary and the actions for the current screen.
    let bar_title = gtk::Label::builder()
        .xalign(0.)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    bar_title.add_css_class("heading");
    bar_title.add_css_class("numeric");
    let bar_detail = gtk::Label::builder()
        .xalign(0.)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    bar_detail.add_css_class("caption");
    let summary = gtk::Box::new(gtk::Orientation::Vertical, 2);
    summary.set_hexpand(true);
    summary.set_valign(gtk::Align::Center);
    summary.append(&bar_title);
    summary.append(&bar_detail);
    let placement =
        gtk::DropDown::from_strings(&["Save beside originals", "Save to editing folder"]);
    placement.set_valign(gtk::Align::Center);
    placement.update_property(&[gtk::accessible::Property::Label("Where converted files go")]);
    placement.connect_selected_notify(on(app, |app, d: &gtk::DropDown| {
        let p = if d.selected() == 0 {
            Placement::Beside
        } else {
            Placement::EditingFolder
        };
        if app.model.borrow().placement != p {
            dispatch(app, Msg::Placement(p));
        }
    }));
    let start = pill("Ingest", true);
    start.set_action_name(Some("win.start"));
    let cancel = pill("Cancel", false);
    cancel.connect_clicked(on(app, |app, _: &gtk::Button| dispatch(app, Msg::Cancel)));
    let eject = pill("Eject Card", false);
    eject.connect_clicked(on(app, |app, _: &gtk::Button| eject_card(app)));
    let retry = pill("Retry Failed", false);
    retry.connect_clicked(on(app, |app, _: &gtk::Button| {
        dispatch(app, Msg::Retry(Scope::Incomplete))
    }));
    let new_session = pill("Done", false);
    new_session.connect_clicked(on(app, |app, _: &gtk::Button| dispatch(app, Msg::Back)));
    let open_editing = pill("Open Editing Folder", true);
    open_editing.connect_clicked(on(app, |app, _: &gtk::Button| {
        open_location(app, DestinationKind::Editing)
    }));
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    actions.set_valign(gtk::Align::Center);
    actions.set_halign(gtk::Align::End);
    actions.append(&placement);
    actions.append(&cancel);
    actions.append(&eject);
    actions.append(&new_session);
    actions.append(&retry);
    actions.append(&open_editing);
    actions.append(&start);
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    bottom.add_css_class("bottom-bar");
    bottom.append(&summary);
    bottom.append(&actions);

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.add_top_bar(&banner);
    view.set_content(Some(&stack));
    view.add_bottom_bar(&bottom);
    view.set_bottom_bar_style(adw::ToolbarStyle::Raised);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&view));
    window.set_content(Some(&toasts));

    // Narrow windows stack the summary above the actions.
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        700.,
        adw::LengthUnit::Sp,
    ));
    narrow.add_setter(
        &bottom,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    narrow.add_setter(
        &actions,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    narrow.add_setter(&actions, "halign", Some(&gtk::Align::Fill.to_value()));
    clips.add_narrow_setters(&narrow);
    window.add_breakpoint(narrow);

    // Dropping a card folder ingests it; any other folder is converted in place.
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    {
        let weak = weak.clone();
        drop.connect_drop(move |_, value, _, _| {
            let (Some(app), Ok(list)) = (weak.upgrade(), value.get::<gtk::gdk::FileList>()) else {
                return false;
            };
            let Some(path) = list.files().first().and_then(|f| f.path()) else {
                return false;
            };
            let folder = if path.is_dir() {
                path
            } else {
                match path.parent() {
                    Some(p) => p.to_path_buf(),
                    None => return false,
                }
            };
            let workflow = if cards::is_card(&folder) {
                Workflow::Ingest
            } else {
                Workflow::Convert
            };
            dispatch(&app, Msg::UseSource(folder, workflow));
            true
        });
    }
    window.add_controller(drop);

    window.connect_close_request(move |window| {
        let Some(app) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        if app.allow_close.get() || app.model.borrow().screen != Screen::Running {
            return glib::Propagation::Proceed;
        }
        dialogs::confirm_close(&app, window);
        glib::Propagation::Stop
    });

    Ui {
        window,
        toasts,
        title,
        banner,
        stack,
        setup_rows,
        cards,
        shown_cards: RefCell::new(vec![]),
        waiting_chips,
        scan_title,
        scan_detail,
        headline,
        subline,
        chips,
        progress,
        progress_box,
        progress_text,
        activity,
        list_tools,
        list_stack,
        empty,
        show_archived,
        clips,
        bar_title,
        bar_detail,
        placement,
        start,
        cancel,
        eject,
        retry,
        open_editing,
        new_session,
        view,
        prefs: RefCell::new(None),
    }
}

fn status_page(
    icon: &str,
    title: &str,
    description: &str,
    child: &impl IsA<gtk::Widget>,
) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(description)
        .child(child)
        .build()
}

fn clamp(child: &impl IsA<gtk::Widget>, width: i32) -> adw::Clamp {
    adw::Clamp::builder()
        .maximum_size(width)
        .child(child)
        .build()
}

fn location_row(app: &Rc<App>, kind: DestinationKind) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(match kind {
            DestinationKind::Archive => "Archive",
            DestinationKind::Editing => "Editing copies",
        })
        .subtitle_lines(2)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(location_icon(kind)));
    let button = gtk::Button::from_icon_name("folder-open-symbolic");
    button.add_css_class("flat");
    button.set_valign(gtk::Align::Center);
    button.set_tooltip_text(Some("Choose Folder"));
    row.add_suffix(&button);
    row.set_activatable_widget(Some(&button));
    button.connect_clicked(on(app, move |app, _: &gtk::Button| {
        dialogs::choose_destination(app, kind)
    }));
    row
}

fn location_icon(kind: DestinationKind) -> &'static str {
    match kind {
        DestinationKind::Archive => "drive-harddisk-symbolic",
        DestinationKind::Editing => "video-x-generic-symbolic",
    }
}

pub(crate) fn location_subtitle(path: Option<&Path>, info: Option<&LocationInfo>) -> String {
    let Some(path) = path else {
        return "Not set".into();
    };
    let mut text = path.display().to_string();
    match info {
        Some(LocationInfo { error: Some(_), .. }) => text.push_str(" · unavailable"),
        Some(LocationInfo {
            free: Some(free),
            kind,
            ..
        }) => text.push_str(&format!(" · {kind} · {} free", human_bytes(*free))),
        _ => (),
    }
    text
}

fn chip(app: &Rc<App>, kind: DestinationKind) -> gtk::Button {
    let content = adw::ButtonContent::builder()
        .icon_name(location_icon(kind))
        .build();
    let button = gtk::Button::builder().child(&content).build();
    button.add_css_class("chip");
    button.connect_clicked(on(app, |app, _: &gtk::Button| dialogs::preferences(app)));
    button
}

fn render_chip(button: &gtk::Button, model: &Model, kind: DestinationKind) {
    let content = button.child().and_downcast::<adw::ButtonContent>().unwrap();
    let (label, info) = match kind {
        DestinationKind::Archive => ("Archive", model.locations.archive.as_ref()),
        DestinationKind::Editing => ("Editing", model.locations.editing.as_ref()),
    };
    let path = state::location_path(model, kind);
    let name = info
        .map(|i| i.name.clone())
        .or_else(|| {
            path.and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Not set".into());
    let detail = match info {
        Some(LocationInfo { error: Some(_), .. }) => " · unavailable".into(),
        Some(LocationInfo {
            free: Some(free), ..
        }) => format!(" · {} free", human_bytes(*free)),
        _ => String::new(),
    };
    content.set_label(&format!("{label} · {name}{detail}"));
    button.set_tooltip_text(Some(&location_subtitle(path, info)));
    if info.is_some_and(|i| i.error.is_some()) {
        button.add_css_class("error");
    } else {
        button.remove_css_class("error");
    }
}

fn filter_popover(app: &Rc<App>) -> gtk::Popover {
    let model = app.model.borrow();
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(6);
    content.set_margin_bottom(6);
    content.set_margin_start(6);
    content.set_margin_end(6);
    if model.workflow() == Workflow::Ingest {
        let archived = gtk::CheckButton::with_label(&format!(
            "Show already archived ({})",
            model.archived_count()
        ));
        archived.set_active(model.filter.show_archived);
        archived.connect_toggled(on(app, |app, c: &gtk::CheckButton| {
            dispatch(app, Msg::ShowArchived(c.is_active()))
        }));
        content.append(&archived);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    }
    let heading = gtk::Label::builder().label("File types").xalign(0.).build();
    heading.add_css_class("heading");
    content.append(&heading);
    for file_type in model.file_types() {
        let check = gtk::CheckButton::with_label(&file_type);
        check.set_active(!model.filter.hidden_types.contains(&file_type));
        check.connect_toggled(on(app, move |app, c: &gtk::CheckButton| {
            dispatch(app, Msg::ShowType(file_type.clone(), c.is_active()))
        }));
        content.append(&check);
    }
    let popover = gtk::Popover::new();
    popover.set_child(Some(&content));
    popover
}

fn install_actions(application: &adw::Application, app: &Rc<App>) {
    let window = &app.ui().window;
    let add = |name: &str, f: fn(&Rc<App>)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(app);
        action.connect_activate(move |_, _| {
            if let Some(app) = weak.upgrade() {
                f(&app);
            }
        });
        window.add_action(&action);
    };
    add("choose-folder", |app| {
        dialogs::choose_source(app, Workflow::Ingest)
    });
    add("convert", |app| {
        dialogs::choose_source(app, Workflow::Convert)
    });
    add("rescan", |app| dispatch(app, Msg::Rescan));
    add("history", |app| dispatch(app, Msg::RequestHistory));
    add("preferences", dialogs::preferences);
    add("about", dialogs::about);
    add("start", |app| dispatch(app, Msg::Start));
    for (action, accels) in [
        ("win.choose-folder", &["<Control>o"][..]),
        ("win.rescan", &["<Control>r", "F5"]),
        ("win.history", &["<Control>h"]),
        ("win.preferences", &["<Control>comma"]),
        ("win.start", &["<Control>Return"]),
        ("window.close", &["<Control>w", "<Control>q"]),
    ] {
        application.set_accels_for_action(action, accels);
    }
}

fn set_action(window: &adw::ApplicationWindow, name: &str, enabled: bool) {
    if let Some(action) = window
        .lookup_action(name)
        .and_downcast::<gio::SimpleAction>()
    {
        action.set_enabled(enabled);
    }
}

fn eject_card(app: &Rc<App>) {
    let Some(card) = app.model.borrow().source_card().cloned() else {
        return;
    };
    if app.demo {
        dispatch(app, Msg::CardsChanged(vec![]));
        toast(app, &format!("{} can be removed", card.name));
        return;
    }
    let weak = Rc::downgrade(app);
    cards::eject(&card.path, move |result| {
        if let Some(app) = weak.upgrade() {
            match result {
                Ok(()) => toast(&app, &format!("{} can be removed", card.name)),
                Err(e) => toast(&app, &format!("Could not eject: {e}")),
            }
        }
    });
}

pub(crate) fn open_location(app: &Rc<App>, kind: DestinationKind) {
    let path = state::location_path(&app.model.borrow(), kind).map(Path::to_path_buf);
    open_path(app, path);
}

pub(crate) fn open_path(app: &Rc<App>, path: Option<PathBuf>) {
    let Some(path) = path else {
        return;
    };
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
    let weak = Rc::downgrade(app);
    launcher.launch(
        Some(&app.ui().window),
        None::<&gio::Cancellable>,
        move |r| {
            if let (Some(app), Err(e)) = (weak.upgrade(), r) {
                toast(&app, &format!("Cannot open folder: {e}"));
            }
        },
    );
}

fn render(app: &Rc<App>) {
    let Some(ui) = app.ui.get() else {
        return;
    };
    let model = app.model.borrow();
    let screen = model.screen;

    // Banner: a persistent problem, otherwise an interrupted-ingest notice.
    if let Some(problem) = &model.problem {
        ui.banner.set_title(problem);
        ui.banner.set_button_label(Some("Dismiss"));
        ui.banner.set_revealed(true);
    } else if model.interrupted > 0 {
        let n = model.interrupted;
        ui.banner.set_title(&if n == 1 {
            "An earlier ingest was interrupted".into()
        } else {
            format!("{n} earlier ingests were interrupted")
        });
        ui.banner.set_button_label(Some("Review"));
        ui.banner.set_revealed(true);
    } else {
        ui.banner.set_revealed(false);
    }

    ui.stack.set_visible_child_name(match screen {
        Screen::Setup => "setup",
        Screen::Waiting => "waiting",
        Screen::Scanning => "scanning",
        Screen::Review | Screen::Running | Screen::Done => "session",
    });
    ui.title.set_subtitle(match &model.source {
        Some(source) if !matches!(screen, Screen::Setup | Screen::Waiting) => &source.name,
        _ => "",
    });

    let window = &ui.window;
    let idle = !model.busy && !model.worker_failed;
    set_action(window, "choose-folder", idle && model.configured());
    set_action(window, "convert", idle);
    set_action(
        window,
        "rescan",
        idle && model.source.is_some() && matches!(screen, Screen::Review | Screen::Done),
    );
    set_action(window, "history", idle);
    set_action(window, "preferences", screen != Screen::Running);
    let (_, _, ready) = model.review_summary();
    set_action(window, "start", screen == Screen::Review && ready && idle);

    let paths = [
        model.settings.archive.as_ref().map(|d| d.path.as_path()),
        model.settings.editing.as_ref().map(|d| d.path.as_path()),
    ];
    let infos = [
        model.locations.archive.as_ref(),
        model.locations.editing.as_ref(),
    ];
    for ((row, path), info) in ui.setup_rows.iter().zip(paths).zip(infos) {
        row.set_subtitle(&location_subtitle(path, info));
    }
    if let Some(rows) = ui.prefs.borrow().as_ref() {
        rows.render(&model);
    }
    match screen {
        Screen::Waiting => {
            render_cards(app, ui, &model);
            render_chip(&ui.waiting_chips[0], &model, DestinationKind::Archive);
            render_chip(&ui.waiting_chips[1], &model, DestinationKind::Editing);
        }
        Screen::Scanning => {
            let name = model.source.as_ref().map(|s| s.name.as_str()).unwrap_or("");
            ui.scan_title.set_text(&if model.canceling {
                "Stopping…".into()
            } else {
                format!("Reading {name}")
            });
            ui.scan_detail.set_text(&if model.scan.0 == 0 {
                "Looking for footage…".into()
            } else {
                format!("{} files checked · {}", model.scan.0, model.scan.1)
            });
        }
        _ => (),
    }
    let session = matches!(screen, Screen::Review | Screen::Running | Screen::Done);
    ui.view.set_reveal_bottom_bars(session);
    // Only the current screen's actions are shown (and measured).
    for w in [
        ui.placement.upcast_ref::<gtk::Widget>(),
        ui.start.upcast_ref(),
        ui.cancel.upcast_ref(),
        ui.eject.upcast_ref(),
        ui.retry.upcast_ref(),
        ui.open_editing.upcast_ref(),
        ui.new_session.upcast_ref(),
    ] {
        w.set_visible(false);
    }
    if session {
        render_session(ui, &model, Instant::now());
    }
}

fn render_cards(app: &Rc<App>, ui: &Ui, model: &Model) {
    if *ui.shown_cards.borrow() == model.cards {
        return;
    }
    *ui.shown_cards.borrow_mut() = model.cards.clone();
    while let Some(child) = ui.cards.first_child() {
        ui.cards.remove(&child);
    }
    ui.cards.set_visible(!model.cards.is_empty());
    for card in &model.cards {
        let row = adw::ActionRow::builder()
            .title(&card.name)
            .subtitle(card.path.display().to_string())
            .activatable(true)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("media-flash-symbolic"));
        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        let path = card.path.clone();
        row.connect_activated(on(app, move |app, _: &adw::ActionRow| {
            dispatch(app, Msg::UseSource(path.clone(), Workflow::Ingest))
        }));
        ui.cards.append(&row);
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn render_session(ui: &Ui, model: &Model, now: Instant) {
    let screen = model.screen;
    let converting = model.workflow() == Workflow::Convert;
    let source_name = model.source.as_ref().map(|s| s.name.as_str()).unwrap_or("");
    match screen {
        Screen::Review => {
            let total = model.candidates.len();
            let archived = model.archived_count();
            if converting {
                ui.headline
                    .set_text(&format!("Convert files in {source_name}"));
                ui.subline.set_text(&format!(
                    "{} found · sources stay unchanged; outputs are MOV with PCM audio",
                    plural(total, "file", "files")
                ));
            } else {
                let new = model.new_count();
                ui.headline.set_text(&if new == 0 && total > 0 {
                    "Nothing new on this card".into()
                } else {
                    format!("{} to ingest", plural(new, "new clip", "new clips"))
                });
                ui.subline.set_text(&if archived == 0 {
                    format!("{} on {source_name}", plural(total, "file", "files"))
                } else {
                    format!(
                        "{} on {source_name} · {archived} already archived",
                        plural(total, "file", "files")
                    )
                });
            }
        }
        Screen::Running => {
            let n = model.batch.as_ref().map(|b| b.items.len()).unwrap_or(0);
            ui.headline.set_text(&if model.canceling {
                "Stopping safely…".into()
            } else if converting {
                format!("Converting {}", plural(n, "file", "files"))
            } else {
                format!("Ingesting {}", plural(n, "file", "files"))
            });
            ui.subline.set_text(if model.canceling {
                "Waiting for active work to stop. Verified files are kept."
            } else if converting {
                "Each copy is checked before it appears in its folder."
            } else {
                "Originals are verified before editing copies appear in the editing folder."
            });
        }
        Screen::Done => {
            let (title, detail) = model.done_summary();
            ui.headline.set_text(&title);
            ui.subline.set_text(&detail);
        }
        _ => (),
    }
    render_chip(&ui.chips[0], model, DestinationKind::Archive);
    ui.chips[0].set_visible(!converting);
    let beside = converting
        && match &model.batch {
            Some(b) if model.showing_batch() => {
                b.settings.editing.as_ref().map(|d| &d.path) == Some(&b.settings.source)
            }
            _ => model.placement == Placement::Beside,
        };
    if beside {
        let content = ui.chips[1]
            .child()
            .and_downcast::<adw::ButtonContent>()
            .unwrap();
        content.set_label("Output · beside originals");
        ui.chips[1].set_tooltip_text(Some("Each clip.mp4 gets a clip.mp4.mov next to it"));
        ui.chips[1].remove_css_class("error");
    } else {
        render_chip(&ui.chips[1], model, DestinationKind::Editing);
    }

    let running = screen == Screen::Running;
    ui.progress_box.set_visible(running);
    let fraction = model.fraction();
    ui.progress.set_fraction(fraction);
    if running {
        let bytes: u64 = model
            .batch
            .as_ref()
            .map(|b| b.items.iter().map(|i| i.source.identity.size).sum())
            .unwrap_or(0);
        let eta = model
            .remaining(now)
            .map(state::remaining_text)
            .unwrap_or_else(|| "estimating time…".into());
        ui.progress_text.set_text(&format!(
            "{:.0}% of {} · {eta}",
            fraction * 100.,
            human_bytes(bytes)
        ));
        let lines = model.activity(now);
        ui.activity.set_text(&if lines.is_empty() {
            "Finishing…".into()
        } else {
            lines.join("   ·   ")
        });
    }
    let empty = screen == Screen::Review && model.visible_count() == 0;
    ui.list_tools
        .set_visible(screen == Screen::Review && !empty);
    ui.list_stack
        .set_visible_child_name(if empty { "empty" } else { "list" });
    if empty {
        let archived = model.archived_count();
        if archived > 0 && !model.filter.show_archived {
            ui.empty.set_title("All caught up");
            ui.empty
                .set_description(Some("Everything here is already verified in the archive."));
            ui.show_archived.set_visible(true);
        } else {
            ui.empty.set_title("No footage found");
            ui.empty.set_description(Some(if converting {
                "This folder has no video files to convert."
            } else {
                "This card or folder has no files to ingest."
            }));
            ui.show_archived.set_visible(false);
        }
    }

    ui.bar_detail.remove_css_class("error");
    ui.bar_detail.add_css_class("dim-label");
    ui.bar_detail.set_tooltip_text(None);
    match screen {
        Screen::Review => {
            let (title, detail, _) = model.review_summary();
            ui.bar_title.set_text(&title);
            ui.bar_detail.set_text(&detail);
            if let Preflight::Failed(e) = &model.preflight {
                ui.bar_detail.remove_css_class("dim-label");
                ui.bar_detail.add_css_class("error");
                ui.bar_detail.set_tooltip_text(Some(e));
            }
            ui.placement.set_visible(converting);
            ui.placement
                .set_selected(u32::from(model.placement == Placement::EditingFolder));
            ui.start.set_visible(true);
            ui.start.set_label(&model.start_label());
        }
        Screen::Running => {
            ui.bar_title
                .set_text(&format!("{:.0}% complete", fraction * 100.));
            ui.bar_detail.set_text(if model.canceling {
                "Canceling · partial files stay marked incomplete"
            } else {
                "You can close the window; it asks before stopping work"
            });
            ui.cancel.set_visible(true);
            ui.cancel.set_sensitive(!model.canceling);
        }
        Screen::Done => {
            let failed = model.failed_count();
            ui.bar_title.set_text(&if failed > 0 {
                format!("{failed} need attention")
            } else {
                "Everything verified".into()
            });
            ui.bar_detail.set_text(if failed > 0 {
                "Open a file for details, or retry the ones that failed"
            } else if converting {
                "Converted files are ready for Resolve"
            } else {
                "Originals are safe in the archive and editing copies are ready for Resolve"
            });
            let card = model.source_card();
            ui.eject.set_visible(card.is_some_and(|c| c.can_eject));
            ui.retry.set_visible(failed > 0);
            ui.retry.set_sensitive(!model.busy);
            // With a card inserted, Rescan lives in the menu and Eject is the next step.
            ui.new_session.set_visible(card.is_none());
            ui.open_editing.set_visible(true);
            ui.open_editing.set_label(if beside {
                "Open Folder"
            } else {
                "Open Editing Folder"
            });
        }
        _ => (),
    }
}
