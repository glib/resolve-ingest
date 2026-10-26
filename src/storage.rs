//! Linux directory-descriptor storage contract. Every descent rejects symlinks and mount crossings.
use crate::settings::Destination;
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{CString, OsStr},
    fs::{self, File},
    io,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct Mount {
    pub id: String,
    pub device: String,
    pub root: PathBuf,
    pub point: PathBuf,
    pub fs_type: String,
    pub source: String,
}
impl Mount {
    /// Mount source as the kernel reports it. Cheap; used to detect remounts during work.
    pub fn stable(&self) -> String {
        format!("{}|{}|{}", self.fs_type, self.source, self.root.display())
    }
    /// Survives `/dev/sdX` renumbering of removable drives. udisks names their mount points
    /// after the volume label or UUID, which also works inside a sandbox without `/dev/disk`.
    pub fn durable_id(&self) -> String {
        if self.source.starts_with("/dev/")
            && (self.point.starts_with("/run/media") || self.point.starts_with("/media"))
        {
            return format!(
                "{}|mount={}|{}",
                self.fs_type,
                self.point.display(),
                self.root.display()
            );
        }
        match block_uuid(&self.source) {
            Some(uuid) => format!("{}|uuid={uuid}|{}", self.fs_type, self.root.display()),
            None => self.stable(),
        }
    }
    /// Accepts identities recorded before UUIDs were used.
    pub fn matches(&self, expected: &str) -> bool {
        expected == self.stable() || expected == self.durable_id()
    }
    pub fn is_network(&self) -> bool {
        is_network_fs(&self.fs_type)
    }
}
pub fn is_network_fs(fs_type: &str) -> bool {
    matches!(fs_type, "cifs" | "smb3" | "nfs" | "nfs4")
}
fn block_uuid(source: &str) -> Option<String> {
    if !source.starts_with("/dev/") {
        return None;
    }
    let device = fs::canonicalize(source).ok()?;
    fs::read_dir("/dev/disk/by-uuid")
        .ok()?
        .flatten()
        .find(|e| fs::canonicalize(e.path()).is_ok_and(|p| p == device))
        .map(|e| e.file_name().to_string_lossy().into_owned())
}
fn unescape(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}
pub fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (a, b) = line.split_once(" - ")?;
            let a: Vec<_> = a.split_whitespace().collect();
            let b: Vec<_> = b.split_whitespace().collect();
            if a.len() < 6 || b.len() < 2 {
                return None;
            }
            Some(Mount {
                id: a[0].into(),
                device: a[2].into(),
                root: unescape(a[3]).into(),
                point: unescape(a[4]).into(),
                fs_type: b[0].into(),
                source: unescape(b[1]),
            })
        })
        .collect()
}
pub fn mount_for(path: &Path) -> Result<Mount> {
    parse_mounts(&fs::read_to_string("/proc/self/mountinfo")?)
        .into_iter()
        .filter(|m| path.starts_with(&m.point))
        .max_by_key(|m| m.point.as_os_str().len())
        .context("Cannot identify the mounted filesystem")
}
fn cstr(s: &OsStr) -> Result<CString> {
    Ok(CString::new(s.as_bytes())?)
}
fn cvt(fd: i32) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a successful open returns a new descriptor owned by this File.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}
fn open_dir_at(parent: &File, name: &OsStr) -> Result<File> {
    let name = cstr(name)?;
    // SAFETY: valid directory descriptor and NUL-terminated name; no pointers are retained.
    Ok(cvt(unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })?)
}
fn absolute_dir(path: &Path) -> Result<File> {
    ensure!(path.is_absolute(), "Location must be absolute");
    let mut fd = File::open("/")?;
    for c in path.components() {
        match c {
            Component::RootDir => (),
            Component::Normal(n) => fd = open_dir_at(&fd, n)?,
            _ => bail!("Unsafe location"),
        }
    }
    Ok(fd)
}

pub struct Root {
    pub path: PathBuf,
    pub dir: File,
    mount: Mount,
    identity: (u64, u64),
}
impl Root {
    pub fn source(path: &Path) -> Result<Self> {
        Self::open(path, None)
    }
    pub fn destination(d: &Destination) -> Result<Self> {
        Self::open(&d.path, Some(d.clone()))
    }
    fn open(path: &Path, expected: Option<Destination>) -> Result<Self> {
        let mount = mount_for(path)?;
        if let Some(d) = &expected {
            ensure!(
                mount.matches(&d.expected_mount),
                "Destination unavailable or share changed. Reconnect the expected drive or share, then retry."
            );
            ensure!(
                !mount.fs_type.starts_with("fuse"),
                "Unsupported FUSE destination; mount the share through the operating system."
            );
            ensure!(
                !d.shared || mount.is_network(),
                "Expected network share is absent. Reconnect the share."
            );
        }
        let dir = absolute_dir(path).context(
            "Cannot open location securely. Select an existing folder without symlinks.",
        )?;
        let m = dir.metadata()?;
        let r = Self {
            path: path.into(),
            dir,
            mount,
            identity: (m.dev(), m.ino()),
        };
        r.validate()?;
        Ok(r)
    }
    pub fn validate(&self) -> Result<()> {
        let current = mount_for(&self.path)?;
        ensure!(
            current.id == self.mount.id && current.stable() == self.mount.stable(),
            "Destination mount changed during work. Reconnect the expected share and retry."
        );
        let m = absolute_dir(&self.path)?.metadata()?;
        ensure!(
            (m.dev(), m.ino()) == self.identity,
            "Location was replaced. Stop and select the mounted folder again."
        );
        Ok(())
    }
    fn parent(&self, relative: &Path, create: bool) -> Result<(File, CString)> {
        self.validate()?;
        let mut parts = relative.components().peekable();
        let mut dir = self.dir.try_clone()?;
        while let Some(c) = parts.next() {
            let Component::Normal(name) = c else {
                bail!("Unsafe relative path: {}", relative.display());
            };
            if parts.peek().is_none() {
                return Ok((dir, cstr(name)?));
            }
            if create {
                let n = cstr(name)?;
                // SAFETY: mkdirat takes a live descriptor and a valid string.
                let result = unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o755) };
                if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
                    return Err(io::Error::last_os_error().into());
                }
                if result == 0 {
                    sync_dir(&dir)?;
                }
            }
            dir = open_dir_at(&dir, name)?;
            ensure!(
                dir.metadata()?.dev() == self.identity.0,
                "Nested mount crossed; choose a single filesystem root"
            );
            // st_dev alone misses same-device bind mounts. fdinfo exposes the actual mount ID.
            let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", dir.as_raw_fd()))?;
            let mid = fdinfo
                .lines()
                .find_map(|l| l.strip_prefix("mnt_id:").map(str::trim))
                .context("Mount ID unavailable")?;
            ensure!(
                mid == self.mount.id,
                "Nested bind mount crossed; select its root explicitly"
            );
        }
        bail!("Empty relative path")
    }
    pub fn read(&self, path: &Path) -> Result<File> {
        let (dir, name) = self.parent(path, false)?;
        // O_NONBLOCK prevents FIFO opens from hanging before the regular-file check.
        let file = cvt(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            )
        })?;
        ensure!(file.metadata()?.is_file(), "Source is not a regular file");
        ensure!(
            file.metadata()?.dev() == self.identity.0,
            "File belongs to a different filesystem"
        );
        let fdinfo = fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?;
        let mid = fdinfo
            .lines()
            .find_map(|l| l.strip_prefix("mnt_id:").map(str::trim));
        ensure!(
            mid == Some(self.mount.id.as_str()),
            "File is a nested mount alias; refusing to ingest it"
        );
        Ok(file)
    }
    pub fn create(&self, path: &Path) -> Result<File> {
        let (dir, name) = self.parent(path, true)?;
        Ok(cvt(unsafe {
            libc::openat(
                dir.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        })?)
    }
    pub fn exists(&self, path: &Path) -> Result<bool> {
        match self.read(path) {
            Ok(_) => Ok(true),
            Err(e)
                if e.downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
    pub fn publish(&self, tmp: &Path, target: &Path) -> Result<bool> {
        self.validate()?;
        let (from, a) = self.parent(tmp, false)?;
        let (to, b) = self.parent(target, false)?;
        // SAFETY: descriptors and strings stay live throughout this same-filesystem syscall.
        let r = unsafe {
            libc::renameat2(
                from.as_raw_fd(),
                a.as_ptr(),
                to.as_raw_fd(),
                b.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if r != 0 {
            return Err(io::Error::last_os_error()).context("Cannot publish without overwriting. Check filesystem support or an existing-file conflict.");
        }
        let supported = sync_dir(&to)?;
        self.validate()?;
        Ok(supported)
    }
    pub fn remove_owned(&self, path: &Path) -> Result<()> {
        ensure!(
            path.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".ri-")
                    && n.to_string_lossy().ends_with(".partial")),
            "Refusing to remove an unowned filename"
        );
        let (dir, name) = self.parent(path, false)?;
        if unsafe { libc::unlinkat(dir.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        sync_dir(&dir)?;
        Ok(())
    }
    pub fn capabilities(&self) -> Result<String> {
        let id = uuid::Uuid::new_v4();
        let a = PathBuf::from(format!(".ri-{id}-a.partial"));
        let b = PathBuf::from(format!(".ri-{id}-b.partial"));
        self.create(&a)?.sync_all()?;
        let result = (|| {
            self.create(&b)?.sync_all()?;
            let conflict = match self.publish(&a, &b) {
                Err(e) => e,
                Ok(_) => bail!(
                    "Storage overwrote a capability test file despite RENAME_NOREPLACE. This filesystem is unsupported."
                ),
            };
            ensure!(
                conflict
                    .downcast_ref::<io::Error>()
                    .is_some_and(|e| e.kind() == io::ErrorKind::AlreadyExists),
                "Storage lacks required no-clobber publication: {conflict:#}"
            );
            self.remove_owned(&b)?;
            let synced = self.publish(&a, &b)?;
            Ok(format!(
                "{}: no-clobber publication checked; directory sync {}",
                self.path.display(),
                if synced {
                    "supported"
                } else {
                    "unsupported (no directory durability promise)"
                }
            ))
        })();
        for p in [&a, &b] {
            if self.exists(p).unwrap_or(false) {
                let _ = self.remove_owned(p);
            }
        }
        result
    }
    pub fn free_bytes(&self) -> Result<u64> {
        Ok(fs2::available_space(format!(
            "/proc/self/fd/{}",
            self.dir.as_raw_fd()
        ))?)
    }
    pub fn device(&self) -> u64 {
        self.identity.0
    }
    pub fn same_filesystem(&self, other: &Self) -> bool {
        self.device() == other.device()
            || (self.mount.is_network()
                && self.mount.fs_type == other.mount.fs_type
                && self.mount.source == other.mount.source)
    }
}
pub fn sync_dir(dir: &File) -> Result<bool> {
    match dir.sync_all() {
        Ok(()) => Ok(true),
        Err(e) if matches!(e.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) => Ok(false),
        Err(e) => Err(e).context("Directory sync failed; publication durability is uncertain"),
    }
}
pub fn reject_overlap(paths: &[&Path]) -> Result<()> {
    let paths: Vec<_> = paths
        .iter()
        .map(fs::canonicalize)
        .collect::<io::Result<_>>()?;
    for (i, a) in paths.iter().enumerate() {
        for b in &paths[i + 1..] {
            ensure!(
                !a.starts_with(b) && !b.starts_with(a),
                "Locations overlap: {} and {}",
                a.display(),
                b.display()
            );
            let am = mount_for(a)?;
            let bm = mount_for(b)?;
            if am.device == bm.device || (am.source == bm.source && am.fs_type == bm.fs_type) {
                let ap = am.root.join(a.strip_prefix(&am.point)?);
                let bp = bm.root.join(b.strip_prefix(&bm.point)?);
                ensure!(
                    !ap.starts_with(&bp) && !bp.starts_with(&ap),
                    "Locations overlap through mount aliases: {} and {}",
                    a.display(),
                    b.display()
                );
            }
        }
    }
    Ok(())
}

/// Keeps camera filenames readable while avoiding characters SMB/exFAT destinations reject.
/// Uniqueness is the planner's job: it compares names case-insensitively.
pub fn portable_name(name: &OsStr) -> Result<String> {
    let name = name
        .to_str()
        .with_context(|| format!("Non-UTF8 filename {name:?}. Rename a copy before ingest."))?;
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    while out.ends_with(['.', ' ']) {
        out.pop();
    }
    ensure!(
        !out.is_empty() && out != "." && out != "..",
        "Unusable filename {name:?}"
    );
    ensure!(
        out.len() <= 240,
        "Filename too long: {name}. Rename a copy of this source before ingest."
    );
    Ok(out)
}
