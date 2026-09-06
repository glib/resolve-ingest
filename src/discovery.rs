use crate::{
    media,
    model::{Cancel, Candidate, Identity},
    storage::Root,
};
use anyhow::{Context, Result};
use std::path::Path;

pub async fn scan(path: &Path, cancel: &Cancel) -> Result<Vec<Candidate>> {
    scan_with_progress(path, cancel, false, |_, _| {}).await
}

/// Conversion scans omit nested Resolve-generated folders. Archive scans retain every original.
/// The callback runs before each probe so even a slow file is identified to the user.
pub async fn scan_with_progress(
    path: &Path,
    cancel: &Cancel,
    exclude_resolve_generated: bool,
    mut progress: impl FnMut(usize, &Path),
) -> Result<Vec<Candidate>> {
    let root =
        Root::source(path).context("Source unavailable. Check access to the selected folder.")?;
    let mut items = Vec::new();
    for entry in walkdir::WalkDir::new(path)
        .follow_links(false)
        .same_file_system(true)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| {
            !(exclude_resolve_generated
                && entry.depth() > 0
                && entry.file_type().is_dir()
                && [
                    "CacheClip",
                    "ProxyMedia",
                    "Proxy",
                    ".gallery",
                    "Resolve Project Backups",
                ]
                .iter()
                .any(|name| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .eq_ignore_ascii_case(name)
                }))
        })
    {
        cancel.check()?;
        let entry = entry.context("Could not enumerate source. Check card access and rescan.")?;
        if !entry.file_type().is_file() {
            continue;
        }
        let relative = entry.path().strip_prefix(path)?.to_path_buf();
        progress(items.len(), &relative);
        let file = root.read(&relative)?;
        let identity = Identity::of(&file.metadata()?);
        let ext = entry
            .path()
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        // Probe unknown files as well, so extensionless or mislabeled video is discovered.
        let result = media::probe(&file, None, cancel).await;
        cancel.check()?;
        let (probe, probe_error) = match result {
            Ok(p) => (Some(p), None),
            Err(e) => (None, Some(format!("{e:#}"))),
        };
        let editing = probe.as_ref().is_some_and(|p| p.video().is_some());
        let candidate = matches!(
            ext.as_str(),
            "mp4" | "mov" | "mxf" | "mts" | "m2ts" | "avi" | "mkv" | "lrf"
        );
        items.push(Candidate {
            relative,
            identity,
            probe,
            probe_error,
            selected: editing || candidate,
            editing,
            sidecar_for: None,
        });
    }
    let videos: Vec<_> = items
        .iter()
        .filter(|i| i.editing)
        .map(|i| i.relative.clone())
        .collect();
    for i in &mut items {
        let ext = i
            .relative
            .extension()
            .unwrap_or_default()
            .to_string_lossy()
            .to_ascii_lowercase();
        if !i.editing && matches!(ext.as_str(), "srt" | "xmp" | "thm" | "xml" | "json") {
            i.sidecar_for = videos
                .iter()
                .find(|v| {
                    v.parent() == i.relative.parent() && v.file_stem() == i.relative.file_stem()
                })
                .cloned();
            i.selected = i.sidecar_for.is_some();
        }
    }
    Ok(items)
}
