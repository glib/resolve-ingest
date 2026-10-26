use crate::model::Batch;
use anyhow::Result;
use std::{fs::OpenOptions, io::Write, path::Path};

pub fn export(batch: &Batch, path: &Path) -> Result<()> {
    // Reports never overwrite an existing file, and include bounded per-attempt diagnostics.
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&serde_json::to_vec_pretty(batch)?)?;
    file.sync_all()?;
    Ok(())
}
pub fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024_u64.pow(4) {
        format!("{:.1} TiB", bytes as f64 / (1024_f64.powi(4)))
    } else if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024_f64.powi(3)))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024_f64.powi(2)))
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.)
    } else {
        format!("{bytes} B")
    }
}
