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
    /// Accepts local disks, removable drives, and kernel-mounted CIFS/NFS shares. The mount's
    /// identity is remembered so an absent drive or share can never become a local folder.
    pub fn select(path: PathBuf) -> Result<Self> {
        let path = fs::canonicalize(path)
            .context("Location unavailable. Select the current mounted folder.")?;
        let mount = crate::storage::mount_for(&path)?;
        ensure!(
            !matches!(mount.fs_type.as_str(), "9p" | "ceph" | "afs"),
            "This network filesystem is unsupported. Select local storage or a CIFS/NFS share mounted by the operating system."
        );
        match mount.fs_type.as_str() {
            "fuse.kio-fuse" | "fuse.gvfsd-fuse" => anyhow::bail!(
                "This is a file manager network location (smb://), not a mounted share. Mount the share through the system, for example with an /etc/fstab entry, then choose its folder."
            ),
            "fuse.portal" => anyhow::bail!(
                "The app has no direct access to this folder, only a sandbox copy. Grant access with `flatpak override --user --filesystem=PATH:rw io.github.resolve_ingest.ResolveIngest`, restart, and choose it again."
            ),
            fs => ensure!(
                !fs.starts_with("fuse"),
                "FUSE locations are unsupported. Mount the drive or share through the operating system, then select its folder."
            ),
        }
        Ok(Self {
            path,
            expected_mount: mount.durable_id(),
            shared: mount.is_network(),
        })
    }
    pub fn name(&self) -> String {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub version: u32,
    pub source: PathBuf,
    pub archive: Option<Destination>,
    pub editing: Option<Destination>,
    pub high_contrast: bool,
    pub text_scale: f64,
    pub workflow: Workflow,
    pub verify: Verify,
}
/// How editing copies are checked. Originals always get a full SHA-256 readback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verify {
    /// Stream structure, formats, timing, and a recorded checksum.
    #[default]
    Fast,
    /// Also decodes every audio and video frame of the editing copy.
    Full,
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
            verify: Verify::Fast,
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
