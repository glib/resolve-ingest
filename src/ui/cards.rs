//! Camera-card detection through GIO's volume monitor (gvfs, or its built-in Unix fallback).
use super::state::Card;
use gtk::{gio, glib, prelude::*};
use std::{path::Path, rc::Rc};

/// Calls `changed` with the current cards now and whenever a mount appears or disappears.
/// Keep the returned monitor alive for as long as notifications are wanted.
pub fn watch(changed: impl Fn(Vec<Card>) + 'static) -> gio::VolumeMonitor {
    let monitor = gio::VolumeMonitor::get();
    let changed = Rc::new(changed);
    let notify = {
        let changed = changed.clone();
        move |monitor: &gio::VolumeMonitor| changed(cards(monitor))
    };
    let n = notify.clone();
    monitor.connect_mount_added(move |m, _| n(m));
    let n = notify.clone();
    monitor.connect_mount_removed(move |m, _| n(m));
    let n = notify.clone();
    monitor.connect_mount_changed(move |m, _| n(m));
    let m = monitor.clone();
    glib::idle_add_local_once(move || notify(&m));
    monitor
}

fn cards(monitor: &gio::VolumeMonitor) -> Vec<Card> {
    monitor
        .mounts()
        .into_iter()
        .filter(|m| !m.is_shadowed())
        .filter_map(|mount| {
            let path = mount.root().path()?;
            is_card(&path).then(|| Card {
                name: mount.name().to_string(),
                can_eject: mount.can_eject() || mount.can_unmount(),
                path,
            })
        })
        .collect()
}

pub fn is_card(path: &Path) -> bool {
    path.join("DCIM").is_dir() || path.join("PRIVATE/M4ROOT").is_dir()
}

/// Ejects (or, for fixed readers, unmounts) the card mounted at `path`.
pub fn eject(path: &Path, done: impl FnOnce(Result<(), String>) + 'static) {
    let monitor = gio::VolumeMonitor::get();
    let Some(mount) = monitor
        .mounts()
        .into_iter()
        .find(|m| m.root().path().as_deref() == Some(path))
    else {
        done(Err("The card is no longer mounted".into()));
        return;
    };
    let finish = move |r: Result<(), glib::Error>| done(r.map_err(|e| e.message().to_string()));
    if mount.can_eject() {
        mount.eject_with_operation(
            gio::MountUnmountFlags::NONE,
            None::<&gio::MountOperation>,
            None::<&gio::Cancellable>,
            finish,
        );
    } else {
        mount.unmount_with_operation(
            gio::MountUnmountFlags::NONE,
            None::<&gio::MountOperation>,
            None::<&gio::Cancellable>,
            finish,
        );
    }
}
