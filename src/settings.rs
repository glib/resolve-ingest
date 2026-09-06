use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::PathBuf,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Destination {
    pub path: PathBuf,
    /// Stable filesystem type, mount source, and filesystem root. Never mount options/credentials.
    pub expected_mount: String,
    pub shared: bool,
}
impl Destination {
    pub fn select(path: PathBuf, require_cifs: bool) -> Result<Self> {
        let path = fs::canonicalize(path)
            .context("Location unavailable. Select the current mounted folder.")?;
        let mount = crate::storage::mount_for(&path)?;
        ensure!(
            !matches!(
                mount.fs_type.as_str(),
                "nfs" | "nfs4" | "9p" | "ceph" | "afs"
            ),
            "This network filesystem is unsupported. Select local storage or an OS-mounted CIFS share."
        );
        ensure!(
            !mount.fs_type.starts_with("fuse"),
            "GVfs/FUSE is unsupported. Select an OS-mounted CIFS folder."
        );
        let shared = mount.fs_type == "cifs" || mount.fs_type == "smb3";
        ensure!(
            !require_cifs || shared,
            "Archive must be on an OS-mounted CIFS share. Mount the share first, then select its folder."
        );
        Ok(Self {
            path,
            expected_mount: mount.stable(),
            shared,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub source: PathBuf,
    pub archive: Option<Destination>,
    pub editing: Option<Destination>,
    pub high_contrast: bool,
    pub text_scale: f64,
    pub workflow: Workflow,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Workflow {
    #[default]
    Ingest,
    Convert,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            source: PathBuf::new(),
            archive: None,
            editing: None,
            high_contrast: false,
            text_scale: 1.0,
            workflow: Workflow::Ingest,
        }
    }
}
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("resolve-ingest")
}
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("resolve-ingest")
}
fn xdg(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback)
        })
}
impl Settings {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("settings.toml");
        if !path.exists() {
            return Ok(Self::default());
        }
        let s: Self = toml::from_str(&fs::read_to_string(path)?)?;
        ensure!(s.version == 1, "Unsupported settings version {}", s.version);
        ensure!(
            s.text_scale.is_finite() && (1.0..=2.0).contains(&s.text_scale),
            "Text scale must be between 1 and 2"
        );
        Ok(s)
    }
    pub fn save(&self) -> Result<()> {
        let dir = config_dir();
        fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("settings-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(toml::to_string_pretty(self)?.as_bytes())?;
        file.sync_all()?;
        fs::rename(tmp, dir.join("settings.toml"))?;
        std::fs::File::open(dir)?.sync_all()?;
        Ok(())
    }
}
