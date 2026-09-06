use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    SourceChanged,
    DestinationUnavailable,
    PermissionDenied,
    InsufficientSpace,
    Conflict,
    ChecksumMismatch,
    MalformedMedia,
    MissingTool,
    Conversion,
    Verification,
    TransientIo,
}
impl Category {
    pub fn next_step(self) -> &'static str {
        match self {
            Self::SourceChanged => {
                "Source removed or changed. Reconnect the card or rescan for a new batch."
            }
            Self::DestinationUnavailable => {
                "Destination unavailable. Reconnect the expected share, then retry."
            }
            Self::PermissionDenied => {
                "Permission denied. Check folder permissions before retrying."
            }
            Self::InsufficientSpace => {
                "Insufficient space. Free space or increase the share quota, then retry."
            }
            Self::Conflict => {
                "Existing-file conflict. Preserve both files and choose a new batch; no file was overwritten."
            }
            Self::ChecksumMismatch => {
                "Checksum mismatch. Investigate the source and storage before retrying."
            }
            Self::MalformedMedia => {
                "Malformed media. Keep the original; try Archive only or another media tool."
            }
            Self::MissingTool => {
                "Missing tool or codec. Install a suitable FFmpeg build, then retry."
            }
            Self::Conversion => {
                "Conversion failed. Inspect details for unsupported streams or damaged media."
            }
            Self::Verification => {
                "Verification failed. The editing copy is incomplete; inspect details and retry."
            }
            Self::TransientIo => "Temporary I/O failure. Waiting briefly before a bounded retry.",
        }
    }
}
pub fn classify(error: &anyhow::Error) -> Category {
    let text = format!("{error:#}").to_lowercase();
    if text.contains("source changed")
        || text.contains("source removed")
        || text.contains("source unavailable")
    {
        return Category::SourceChanged;
    }
    if text.contains("share")
        && (text.contains("unavailable") || text.contains("changed") || text.contains("absent"))
    {
        return Category::DestinationUnavailable;
    }
    if let Some(e) = error.downcast_ref::<io::Error>() {
        match e.raw_os_error() {
            Some(libc::ENOSPC | libc::EDQUOT) => return Category::InsufficientSpace,
            Some(
                libc::ESTALE | libc::ENODEV | libc::ENOTCONN | libc::EHOSTDOWN | libc::EHOSTUNREACH,
            ) => return Category::DestinationUnavailable,
            _ => (),
        }
        match e.kind() {
            io::ErrorKind::PermissionDenied => return Category::PermissionDenied,
            io::ErrorKind::AlreadyExists => return Category::Conflict,
            io::ErrorKind::Interrupted
            | io::ErrorKind::TimedOut
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::WouldBlock => return Category::TransientIo,
            _ => (),
        }
    }
    if text.contains("checksum mismatch") {
        Category::ChecksumMismatch
    } else if text.contains("insufficient") {
        Category::InsufficientSpace
    } else if text.contains("conflict")
        || text.contains("existing archive")
        || text.contains("existing editing")
    {
        Category::Conflict
    } else if text.contains("cannot start") || text.contains("lacks") {
        Category::MissingTool
    } else if text.contains("ffprobe failed") || text.contains("no media") {
        Category::MalformedMedia
    } else if text.contains("ffmpeg failed") {
        Category::Conversion
    } else {
        Category::Verification
    }
}
