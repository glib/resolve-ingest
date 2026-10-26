//! Virtualized clip table grouped into recording-day sections. Rows hold fixed scan metadata;
//! selection and status are read from the model whenever a row is bound.
use super::{
    App, dispatch,
    state::{ClipMeta, Model, Msg, Tone},
};
use crate::diagnostics::human_bytes;
use chrono::{Datelike, NaiveDate};
use gtk::{gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
};

/// Cell widgets currently bound to each row index, so progress updates rebind in place
/// rather than churning list items.
type Bound = Rc<RefCell<HashMap<usize, Vec<(Column, glib::WeakRef<gtk::Widget>)>>>>;

pub(super) struct Clips {
    pub widget: gtk::ScrolledWindow,
    view: gtk::ColumnView,
    header_factory: gtk::SignalListItemFactory,
    store: gio::ListStore,
    filter: gtk::CustomFilter,
    headers: Rc<RefCell<Vec<Header>>>,
    bound: Bound,
    /// Set briefly after a rebuild so the first layout lands at the top, header included.
    scroll_top: Rc<Cell<bool>>,
    /// Length, size, details, and status: trimmed in narrow windows.
    secondary: [gtk::ColumnViewColumn; 4],
}

struct Header {
    check: glib::WeakRef<gtk::CheckButton>,
    title: glib::WeakRef<gtk::Label>,
    detail: glib::WeakRef<gtk::Label>,
    day: Rc<Cell<Option<NaiveDate>>>,
    /// Set while the model is written into the widgets, so programmatic changes are not
    /// mistaken for clicks (a partly selected day is shown inactive and inconsistent).
    updating: Rc<Cell<bool>>,
}

fn meta(object: &glib::Object) -> ClipMeta {
    object
        .downcast_ref::<glib::BoxedAnyObject>()
        .unwrap()
        .borrow::<ClipMeta>()
        .clone()
}

fn item_meta(item: &gtk::ListItem) -> Option<ClipMeta> {
    item.item().map(|o| meta(&o))
}

fn ordering(order: std::cmp::Ordering) -> gtk::Ordering {
    match order {
        std::cmp::Ordering::Less => gtk::Ordering::Smaller,
        std::cmp::Ordering::Equal => gtk::Ordering::Equal,
        std::cmp::Ordering::Greater => gtk::Ordering::Larger,
    }
}

pub fn duration_text(seconds: f64) -> String {
    let s = seconds.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

pub fn day_title(day: NaiveDate) -> String {
    if day.year() == chrono::Local::now().year() {
        day.format("%A %-d %B").to_string()
    } else {
        day.format("%A %-d %B %Y").to_string()
    }
}

fn set_tone(widget: &impl IsA<gtk::Widget>, tone: Tone) {
    for class in ["dim-label", "accent", "success", "warning", "error"] {
        widget.remove_css_class(class);
    }
    let class = match tone {
        Tone::Neutral => return,
        Tone::Dim => "dim-label",
        Tone::Accent => "accent",
        Tone::Good => "success",
        Tone::Warn => "warning",
        Tone::Bad => "error",
    };
    widget.add_css_class(class);
}

impl Clips {
    pub fn new(app: &Weak<App>) -> Self {
        let store = gio::ListStore::new::<glib::BoxedAnyObject>();
        let filter = {
            let app = app.clone();
            gtk::CustomFilter::new(move |object| {
                let Some(app) = app.upgrade() else {
                    return true;
                };
                let model = app.model.borrow();
                model.visible(&meta(object))
            })
        };
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        let sorted = gtk::SortListModel::new(Some(filtered), None::<gtk::Sorter>);
        // Newest day first; columns sort within each day.
        sorted.set_section_sorter(Some(&gtk::CustomSorter::new(|a, b| {
            ordering(meta(b).day.cmp(&meta(a).day))
        })));
        let view = gtk::ColumnView::new(Some(gtk::NoSelection::new(Some(sorted.clone()))));
        view.add_css_class("clips");
        view.set_reorderable(false);
        view.set_tab_behavior(gtk::ListTabBehavior::Item);
        let headers = Rc::new(RefCell::new(Vec::new()));
        let header_factory = header_factory(app, &headers);
        view.set_header_factory(Some(&header_factory));

        let bound = Bound::default();
        let name = column(app, &bound, "Name", Column::Name);
        name.set_expand(true);
        let time = column(app, &bound, "Time", Column::Time);
        let duration = column(app, &bound, "Length", Column::Duration);
        let size = column(app, &bound, "Size", Column::Size);
        let status = column(app, &bound, "Status", Column::Status);
        for (c, width) in [(&time, 84), (&duration, 84), (&size, 96), (&status, 220)] {
            c.set_fixed_width(width);
        }
        for c in [&name, &time, &duration, &size, &status] {
            c.set_resizable(true);
            view.append_column(c);
        }
        let details = column(app, &bound, "", Column::Details);
        details.set_fixed_width(48);
        view.append_column(&details);
        sorted.set_sorter(view.sorter().as_ref());
        view.sort_by_column(Some(&time), gtk::SortType::Descending);
        {
            let app = app.clone();
            view.connect_activate(move |_, position| {
                if let (Some(app), Some(object)) = (app.upgrade(), sorted.item(position)) {
                    super::dialogs::details(&app, meta(&object).index);
                }
            });
        }
        let widget = gtk::ScrolledWindow::builder()
            .child(&view)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .build();
        widget.add_css_class("clip-frame");
        let scroll_top = Rc::new(Cell::new(false));
        {
            // GTK restores its scroll anchor (the first row, not its header) during layout,
            // after any idle callback; correct the adjustment as layout reports it.
            let pin = |adj: &gtk::Adjustment, pending: &Rc<Cell<bool>>| {
                if pending.get() && adj.value() != 0. {
                    adj.set_value(0.);
                }
            };
            let adjustment = widget.vadjustment();
            let pending = scroll_top.clone();
            adjustment.connect_changed(move |adj| pin(adj, &pending));
            let pending = scroll_top.clone();
            adjustment.connect_value_changed(move |adj| pin(adj, &pending));
        }
        Self {
            widget,
            view,
            header_factory,
            store,
            filter,
            headers,
            bound,
            scroll_top,
            secondary: [duration, size, details, status],
        }
    }

    /// Narrow windows keep name, time, and a shorter status.
    pub fn add_narrow_setters(&self, breakpoint: &adw::Breakpoint) {
        let [duration, size, details, status] = &self.secondary;
        for column in [duration, size, details] {
            breakpoint.add_setter(column, "visible", Some(&false.to_value()));
        }
        breakpoint.add_setter(status, "fixed-width", Some(&150.to_value()));
        // GTK 4.22 adds cells for re-shown columns to section headers as well; recreating the
        // header widgets after the columns change removes those stray cells.
        let view = self.view.downgrade();
        let factory = self.header_factory.clone();
        // Deferred: `unapply` is emitted before the setters are reverted.
        let reset = move |_: &adw::Breakpoint| {
            let (view, factory) = (view.clone(), factory.clone());
            glib::idle_add_local_once(move || {
                if let Some(view) = view.upgrade() {
                    view.set_header_factory(None::<&gtk::ListItemFactory>);
                    view.set_header_factory(Some(&factory));
                }
            });
        };
        breakpoint.connect_apply(reset.clone());
        breakpoint.connect_unapply(reset);
    }

    pub fn rebuild(&self, model: &Model) {
        let rows: Vec<_> = model
            .rows()
            .iter()
            .map(|m| glib::BoxedAnyObject::new(m.clone()))
            .collect();
        self.store.splice(0, self.store.n_items(), &rows);
        self.refresh_headers(model);
        self.widget.vadjustment().set_value(0.);
        self.scroll_top.set(true);
        let pending = self.scroll_top.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(300), move || {
            pending.set(false)
        });
    }

    /// Updates one row's visible cells (status, checkbox, progress) and the day headers.
    pub fn refresh(&self, model: &Model, index: usize) {
        if let Some(meta) = model.rows().get(index) {
            let cells: Vec<_> = self
                .bound
                .borrow()
                .get(&index)
                .into_iter()
                .flatten()
                .filter_map(|(kind, w)| w.upgrade().map(|w| (*kind, w)))
                .collect();
            for (kind, widget) in cells {
                bind_cell(kind, &widget, meta, model);
            }
        }
        self.refresh_headers(model);
    }

    /// Rebinds every row and re-applies the filter, keeping the scroll position.
    pub fn refresh_all(&self, model: &Model) {
        let rows: Vec<_> = model
            .rows()
            .iter()
            .map(|m| glib::BoxedAnyObject::new(m.clone()))
            .collect();
        self.store.splice(0, self.store.n_items(), &rows);
        self.filter.changed(gtk::FilterChange::Different);
        self.refresh_headers(model);
    }

    fn refresh_headers(&self, model: &Model) {
        self.headers
            .borrow_mut()
            .retain(|h| h.check.upgrade().is_some());
        for header in self.headers.borrow().iter() {
            if let Some(day) = header.day.get() {
                bind_header(header, model, day);
            }
        }
    }
}

fn bind_header(header: &Header, model: &Model, day: NaiveDate) {
    let (selected, total, bytes) = model.day_selection(day);
    header.updating.set(true);
    if let Some(check) = header.check.upgrade() {
        check.set_active(total > 0 && selected == total);
        check.set_inconsistent(selected > 0 && selected < total);
        check.set_visible(model.can_select());
        check.update_property(&[gtk::accessible::Property::Label(&format!(
            "Select all clips from {}",
            day_title(day)
        ))]);
    }
    if let Some(title) = header.title.upgrade() {
        title.set_text(&day_title(day));
    }
    if let Some(detail) = header.detail.upgrade() {
        let noun = if total == 1 { "file" } else { "files" };
        detail.set_text(&format!("{total} {noun} · {}", human_bytes(bytes)));
    }
    header.updating.set(false);
}

fn header_factory(
    app: &Weak<App>,
    headers: &Rc<RefCell<Vec<Header>>>,
) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    let registry = headers.clone();
    let setup_app = app.clone();
    factory.connect_setup(move |_, object| {
        let header = object.downcast_ref::<gtk::ListHeader>().unwrap();
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("day-header");
        let check = gtk::CheckButton::new();
        let title = gtk::Label::new(None);
        title.add_css_class("heading");
        let detail = gtk::Label::new(None);
        detail.add_css_class("dim-label");
        detail.add_css_class("numeric");
        row.append(&check);
        row.append(&title);
        row.append(&detail);
        header.set_child(Some(&row));
        let day = Rc::new(Cell::new(None));
        let updating = Rc::new(Cell::new(false));
        {
            let app = setup_app.clone();
            let day = day.clone();
            let updating = updating.clone();
            check.connect_toggled(move |check| {
                let (Some(app), Some(day)) = (app.upgrade(), day.get()) else {
                    return;
                };
                if updating.get() {
                    return;
                }
                let (selected, total, _) = app.model.borrow().day_selection(day);
                let on = check.is_active();
                if (on && selected == total) || (!on && selected == 0) {
                    return;
                }
                dispatch(&app, Msg::ToggleDay(day, on));
            });
        }
        registry.borrow_mut().push(Header {
            check: check.downgrade(),
            title: title.downgrade(),
            detail: detail.downgrade(),
            day,
            updating,
        });
    });
    let registry = headers.clone();
    let bind_app = app.clone();
    factory.connect_bind(move |_, object| {
        let header = object.downcast_ref::<gtk::ListHeader>().unwrap();
        let (Some(app), Some(item)) = (bind_app.upgrade(), header.item()) else {
            return;
        };
        let day = meta(&item).day;
        let Some(row) = header.child() else {
            return;
        };
        let model = app.model.borrow();
        for h in registry.borrow().iter() {
            if h.check.upgrade().and_then(|c| c.parent()).as_ref() == Some(&row) {
                h.day.set(Some(day));
                bind_header(h, &model, day);
            }
        }
    });
    factory
}

/// Writes the model's view of one row into a cell widget.
fn bind_cell(kind: Column, child: &gtk::Widget, meta: &ClipMeta, model: &Model) {
    match kind {
        Column::Name => {
            let check = child
                .first_child()
                .and_downcast::<gtk::CheckButton>()
                .unwrap();
            let label = check.next_sibling().and_downcast::<gtk::Label>().unwrap();
            check.set_active(model.is_selected(meta.index));
            check.set_visible(!model.showing_batch());
            check.set_sensitive(model.can_select());
            check.update_property(&[gtk::accessible::Property::Label(&format!(
                "Select {}",
                meta.name
            ))]);
            let sidecar = model
                .candidates
                .get(meta.index)
                .is_some_and(|c| c.sidecar_for.is_some())
                && !model.showing_batch();
            label.set_text(&meta.name);
            label.set_tooltip_text(Some(&meta.name));
            if sidecar {
                label.add_css_class("dim-label");
            } else {
                label.remove_css_class("dim-label");
            }
        }
        Column::Time => {
            let label = child.downcast_ref::<gtk::Label>().unwrap();
            let estimated = if meta.estimated_date { " *" } else { "" };
            label.set_text(&format!("{}{estimated}", meta.time.format("%H:%M")));
            label.set_tooltip_text(Some(&format!(
                "{} {}\n{}",
                meta.day,
                meta.time.format("%H:%M:%S"),
                meta.date_origin
            )));
        }
        Column::Duration => {
            let label = child.downcast_ref::<gtk::Label>().unwrap();
            label.set_text(
                &meta
                    .duration
                    .map(duration_text)
                    .unwrap_or_else(|| "—".into()),
            );
        }
        Column::Size => {
            let label = child.downcast_ref::<gtk::Label>().unwrap();
            label.set_text(&human_bytes(meta.size));
        }
        Column::Status => {
            let status = model.status(meta.index);
            let icon = child.first_child().and_downcast::<gtk::Image>().unwrap();
            let label = icon.next_sibling().and_downcast::<gtk::Label>().unwrap();
            let bar = label
                .next_sibling()
                .and_downcast::<gtk::ProgressBar>()
                .unwrap();
            icon.set_icon_name(Some(status.icon));
            icon.set_visible(!status.icon.is_empty());
            label.set_text(&status.text);
            set_tone(&label, status.tone);
            set_tone(&icon, status.tone);
            bar.set_visible(status.progress.is_some());
            bar.set_fraction(status.progress.unwrap_or(0.));
            child.set_tooltip_text(status.tooltip.as_deref());
        }
        Column::Details => (),
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Column {
    Name,
    Time,
    Duration,
    Size,
    Status,
    Details,
}

fn column(app: &Weak<App>, bound: &Bound, title: &str, kind: Column) -> gtk::ColumnViewColumn {
    let factory = gtk::SignalListItemFactory::new();
    let setup_app = app.clone();
    factory.connect_setup(move |_, object| {
        let item = object.downcast_ref::<gtk::ListItem>().unwrap();
        item.set_activatable(true);
        match kind {
            Column::Name => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
                let check = gtk::CheckButton::new();
                let label = gtk::Label::builder()
                    .xalign(0.)
                    .ellipsize(gtk::pango::EllipsizeMode::Middle)
                    .hexpand(true)
                    .build();
                row.append(&check);
                row.append(&label);
                item.set_child(Some(&row));
                let app = setup_app.clone();
                let weak = item.downgrade();
                check.connect_toggled(move |check| {
                    let (Some(app), Some(item)) = (app.upgrade(), weak.upgrade()) else {
                        return;
                    };
                    let Some(meta) = item_meta(&item) else {
                        return;
                    };
                    if app.model.borrow().is_selected(meta.index) == check.is_active() {
                        return;
                    }
                    dispatch(&app, Msg::Toggle(meta.index, check.is_active()));
                });
            }
            Column::Status => {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let icon = gtk::Image::new();
                let label = gtk::Label::builder()
                    .xalign(0.)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .build();
                let bar = gtk::ProgressBar::builder()
                    .valign(gtk::Align::Center)
                    .hexpand(true)
                    .build();
                bar.add_css_class("row-progress");
                row.append(&icon);
                row.append(&label);
                row.append(&bar);
                item.set_child(Some(&row));
            }
            Column::Details => {
                let button = gtk::Button::from_icon_name("go-next-symbolic");
                button.add_css_class("flat");
                button.add_css_class("circular");
                button.set_valign(gtk::Align::Center);
                button.set_tooltip_text(Some("Details"));
                item.set_child(Some(&button));
                let app = setup_app.clone();
                let weak = item.downgrade();
                button.connect_clicked(move |_| {
                    if let (Some(app), Some(item)) = (app.upgrade(), weak.upgrade())
                        && let Some(meta) = item_meta(&item)
                    {
                        super::dialogs::details(&app, meta.index);
                    }
                });
            }
            _ => {
                let label = gtk::Label::builder().xalign(1.).build();
                label.add_css_class("numeric");
                item.set_child(Some(&label));
            }
        }
    });
    let bind_app = app.clone();
    let registry = bound.clone();
    factory.connect_bind(move |_, object| {
        let item = object.downcast_ref::<gtk::ListItem>().unwrap();
        let (Some(app), Some(meta), Some(child)) =
            (bind_app.upgrade(), item_meta(item), item.child())
        else {
            return;
        };
        bind_cell(kind, &child, &meta, &app.model.borrow());
        registry
            .borrow_mut()
            .entry(meta.index)
            .or_default()
            .push((kind, child.downgrade()));
    });
    let registry = bound.clone();
    factory.connect_unbind(move |_, object| {
        let item = object.downcast_ref::<gtk::ListItem>().unwrap();
        let (Some(meta), Some(child)) = (item_meta(item), item.child()) else {
            return;
        };
        let mut registry = registry.borrow_mut();
        if let Some(cells) = registry.get_mut(&meta.index) {
            cells.retain(|(_, w)| w.upgrade().is_some_and(|w| w != child));
            if cells.is_empty() {
                registry.remove(&meta.index);
            }
        }
    });
    let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
    let sorter = match kind {
        Column::Name => Some(gtk::CustomSorter::new(|a, b| {
            ordering(meta(a).name.cmp(&meta(b).name))
        })),
        Column::Time => Some(gtk::CustomSorter::new(|a, b| {
            let (a, b) = (meta(a), meta(b));
            ordering((a.day, a.time, &a.name).cmp(&(b.day, b.time, &b.name)))
        })),
        Column::Duration => Some(gtk::CustomSorter::new(|a, b| {
            ordering(
                meta(a)
                    .duration
                    .unwrap_or(0.)
                    .total_cmp(&meta(b).duration.unwrap_or(0.)),
            )
        })),
        Column::Size => Some(gtk::CustomSorter::new(|a, b| {
            ordering(meta(a).size.cmp(&meta(b).size))
        })),
        Column::Status | Column::Details => None,
    };
    column.set_sorter(sorter.as_ref());
    column
}
