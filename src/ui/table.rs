use super::*;
use crate::catalog;

pub(super) fn build(v: &View) -> gtk::ColumnView {
    let sorted = gtk::SortListModel::new(Some(v.store.clone()), None::<gtk::Sorter>);
    let selection = gtk::NoSelection::new(Some(sorted.clone()));
    let table = gtk::ColumnView::new(Some(selection));
    table.add_css_class("footage-table");
    table.set_show_row_separators(true);
    table.set_vexpand(true);
    for (kind, (title, width)) in [
        ("File", 180),
        ("Recorded", 155),
        ("Type", 60),
        ("Size", 85),
        ("Duration", 70),
        ("Status", 180),
        ("", 72),
        ("Output file", 200),
    ]
    .into_iter()
    .enumerate()
    {
        let factory = gtk::SignalListItemFactory::new();
        let setup_view = v.clone();
        factory.connect_setup(move |_, object| {
            let li = object.downcast_ref::<gtk::ListItem>().unwrap();
            if kind == 0 {
                let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
                let check = gtk::CheckButton::new();
                let name = label("", "");
                ellipsize(&name);
                row.append(&check);
                row.append(&name);
                li.set_child(Some(&row));
                let weak = li.downgrade();
                let v = setup_view.clone();
                check.connect_toggled(move |check| {
                    let Some(li) = weak.upgrade() else {
                        return;
                    };
                    let Some(object) = li.item().and_downcast::<glib::BoxedAnyObject>() else {
                        return;
                    };
                    let row = object.borrow::<Row>();
                    let index = row.index;
                    if row.item.is_some() || v.state.borrow().busy {
                        return;
                    }
                    if v.state
                        .borrow()
                        .candidates
                        .get(index)
                        .is_none_or(|c| c.selected == check.is_active())
                    {
                        return;
                    }
                    drop(row);
                    v.state.borrow_mut().candidates[index].selected = check.is_active();
                    object.borrow_mut::<Row>().candidate.selected = check.is_active();
                    refresh(&v);
                });
            } else if kind == 6 {
                let button = gtk::Button::with_label("Details");
                button.add_css_class("table-action");
                li.set_child(Some(&button));
                let weak = li.downgrade();
                let v = setup_view.clone();
                button.connect_clicked(move |_| {
                    if let Some(li) = weak.upgrade()
                        && let Some(object) = li.item().and_downcast::<glib::BoxedAnyObject>()
                    {
                        details_dialog(&v, object.borrow::<Row>().clone());
                    }
                });
            } else {
                let text = label("", if kind == 5 { "status" } else { "" });
                ellipsize(&text);
                li.set_child(Some(&text));
            }
        });
        let bind_view = v.clone();
        factory.connect_bind(move |_, object| {
            let li = object.downcast_ref::<gtk::ListItem>().unwrap();
            let object = li.item().and_downcast::<glib::BoxedAnyObject>().unwrap();
            let data = object.borrow::<Row>();
            let c = &data.candidate;
            if kind == 0 {
                let row = li.child().and_downcast::<gtk::Box>().unwrap();
                let check = row
                    .first_child()
                    .and_downcast::<gtk::CheckButton>()
                    .unwrap();
                check.set_active(c.selected);
                check.set_sensitive(data.item.is_none());
                check.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Select {}",
                    c.relative.display()
                ))]);
                let name = check.next_sibling().and_downcast::<gtk::Label>().unwrap();
                name.set_text(&c.relative.to_string_lossy());
                name.set_tooltip_text(Some(&c.relative.to_string_lossy()));
            } else if kind != 6 {
                let text = li.child().and_downcast::<gtk::Label>().unwrap();
                let value = match kind {
                    1 => {
                        let recorded = catalog::recorded(c);
                        text.set_tooltip_text(Some(&format!(
                            "{}\n{}",
                            recorded.date, recorded.origin
                        )));
                        format!(
                            "{}{}",
                            recorded.date.format("%Y-%m-%d %H:%M"),
                            if recorded.origin.starts_with("File") {
                                " *"
                            } else {
                                ""
                            }
                        )
                    }
                    2 => catalog::file_type(c),
                    3 => human_bytes(c.identity.size),
                    4 => c
                        .probe
                        .as_ref()
                        .map(|p| format!("{:.1}s", p.duration()))
                        .unwrap_or_else(|| "—".into()),
                    7 => {
                        if let Some(item) = &data.item {
                            let path = item.editing_path.as_ref().unwrap_or(&item.archive_path);
                            text.set_tooltip_text(Some(&format!(
                                "{}\nSee Details for all absolute paths",
                                path.display()
                            )));
                            path.file_name()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .to_string()
                        } else {
                            String::new()
                        }
                    }
                    _ => {
                        if let Some(item) = &data.item {
                            render_status(&text, item);
                        } else {
                            text.set_text(if c.probe_error.is_some() {
                                "Probe failed"
                            } else if c.editing {
                                "Video"
                            } else {
                                "Archive only"
                            });
                            text.set_tooltip_text(c.probe_error.as_deref());
                        }
                        bind_view
                            .status_labels
                            .borrow_mut()
                            .insert(data.index, text);
                        return;
                    }
                };
                text.set_text(&value);
            }
        });
        let unbind = v.clone();
        factory.connect_unbind(move |_, object| {
            if kind == 5 {
                let li = object.downcast_ref::<gtk::ListItem>().unwrap();
                if let Some(object) = li.item().and_downcast::<glib::BoxedAnyObject>() {
                    unbind
                        .status_labels
                        .borrow_mut()
                        .remove(&object.borrow::<Row>().index);
                }
            }
        });
        let column = gtk::ColumnViewColumn::new(Some(title), Some(factory));
        if kind == 7 {
            column.set_visible(false);
            *v.output_column.borrow_mut() = column.downgrade();
        }
        column.set_fixed_width(width);
        column.set_resizable(true);
        if kind == 0 || kind == 5 {
            column.set_expand(true);
        }
        if kind < 5 {
            let sorter = gtk::CustomSorter::new(move |a, b| {
                let a = a
                    .downcast_ref::<glib::BoxedAnyObject>()
                    .unwrap()
                    .borrow::<Row>();
                let b = b
                    .downcast_ref::<glib::BoxedAnyObject>()
                    .unwrap()
                    .borrow::<Row>();
                let (a, b) = (&a.candidate, &b.candidate);
                let order = match kind {
                    0 => a.relative.cmp(&b.relative),
                    1 => catalog::recorded(a).date.cmp(&catalog::recorded(b).date),
                    2 => catalog::file_type(a).cmp(&catalog::file_type(b)),
                    3 => a.identity.size.cmp(&b.identity.size),
                    _ => a
                        .probe
                        .as_ref()
                        .map(|p| p.duration())
                        .unwrap_or(0.)
                        .total_cmp(&b.probe.as_ref().map(|p| p.duration()).unwrap_or(0.)),
                }
                .then_with(|| a.relative.cmp(&b.relative));
                match order {
                    std::cmp::Ordering::Less => gtk::Ordering::Smaller,
                    std::cmp::Ordering::Equal => gtk::Ordering::Equal,
                    std::cmp::Ordering::Greater => gtk::Ordering::Larger,
                }
            });
            column.set_sorter(Some(&sorter));
        }
        if kind == 7 {
            table.insert_column(1, &column);
        } else {
            table.append_column(&column);
        }
        if kind == 1 {
            table.sort_by_column(Some(&column), gtk::SortType::Descending);
        }
    }
    sorted.set_sorter(table.sorter().as_ref());
    table
}
