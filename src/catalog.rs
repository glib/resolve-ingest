//! Display metadata for scanned files. Filesystem ctime is not a recording date.
use crate::model::Candidate;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use sha2::{Digest, Sha256};
use std::{fs::File, os::unix::fs::FileExt};

const SAMPLE: u64 = 1024 * 1024;

/// Size and SHA-256 of the first and last MiB. Reads at most 2 MiB, so a whole card can be
/// compared with earlier ingests quickly; copies still get a full SHA-256.
pub fn fingerprint(file: &File) -> std::io::Result<String> {
    let size = file.metadata()?.len();
    let mut sha = Sha256::new();
    sha.update(size.to_le_bytes());
    let mut buffer = vec![0; SAMPLE as usize];
    for offset in [0, size.saturating_sub(SAMPLE).max(SAMPLE.min(size))] {
        let want = SAMPLE.min(size - offset) as usize;
        file.read_exact_at(&mut buffer[..want], offset)?;
        sha.update(&buffer[..want]);
    }
    Ok(format!("v1:{size}:{:x}", sha.finalize()))
}

pub struct Recorded {
    pub date: NaiveDateTime,
    pub origin: &'static str,
}
pub fn file_type(c: &Candidate) -> String {
    c.relative
        .extension()
        .map(|s| s.to_string_lossy().to_uppercase())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No extension".into())
}
/// Folder date for archive and editing layouts.
pub fn recorded_day(c: &Candidate) -> NaiveDate {
    recorded(c).date.date()
}
pub fn recorded(c: &Candidate) -> Recorded {
    let name = c.relative.file_stem().unwrap_or_default().to_string_lossy();
    let groups: Vec<_> = name
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .collect();
    for (i, group) in groups.iter().enumerate() {
        let stamp = if group.len() == 14 {
            Some(group.to_string())
        } else if group.len() == 8 && groups.get(i + 1).is_some_and(|s| s.len() == 6) {
            Some(format!("{group}{}", groups[i + 1]))
        } else {
            None
        };
        if let Some(stamp) = stamp
            && let Ok(date) = NaiveDateTime::parse_from_str(&stamp, "%Y%m%d%H%M%S")
        {
            return Recorded {
                date,
                origin: "Recording date from filename · camera clock, timezone unspecified",
            };
        }
    }
    if let Some(probe) = &c.probe {
        let dates = std::iter::once(&probe.format["tags"]["creation_time"])
            .chain(probe.streams.iter().map(|s| &s.tags["creation_time"]));
        for value in dates {
            if let Some(text) = value.as_str()
                && let Ok(date) = DateTime::parse_from_rfc3339(text)
            {
                return Recorded {
                    date: date.naive_utc(),
                    origin: "Creation date embedded in media · UTC",
                };
            }
        }
    }
    Recorded {
        date: DateTime::<Utc>::from_timestamp(c.identity.mtime, 0)
            .unwrap_or_default()
            .naive_utc(),
        origin: "File modification date · UTC · recording date unavailable",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(name: &str) -> Candidate {
        Candidate {
            relative: name.into(),
            probe: Some(serde_json::from_value(serde_json::json!({"streams":[],"format":{"tags":{"creation_time":"2025-01-02T03:04:05Z"}}})).unwrap()),
            selected: true,
            editing: true,
            ..Default::default()
        }
    }
    #[test]
    fn recording_dates_prefer_valid_filename_then_media_then_modified() {
        for name in ["DJI_20260826184226_0027_D.MP4", "20260826_184226.mov"] {
            let c = candidate(name);
            assert_eq!(recorded(&c).date.to_string(), "2026-08-26 18:42:26");
            assert!(recorded(&c).origin.contains("filename"));
        }
        let mut c = candidate("DJI_20269999184226.MP4");
        assert_eq!(recorded(&c).date.to_string(), "2025-01-02 03:04:05");
        assert!(recorded(&c).origin.contains("embedded"));
        c.probe = None;
        assert_eq!(recorded(&c).date.to_string(), "1970-01-01 00:00:00");
        assert!(recorded(&c).origin.contains("modification"));
        assert_eq!(file_type(&candidate("clip.mP4")), "MP4");
        assert_eq!(file_type(&candidate("extensionless")), "No extension");
    }
    #[test]
    fn fingerprints_cover_size_head_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let print = |bytes: &[u8]| {
            let path = dir.path().join("f");
            std::fs::write(&path, bytes).unwrap();
            fingerprint(&File::open(path).unwrap()).unwrap()
        };
        let big = vec![7u8; 3 * SAMPLE as usize];
        let mut tail = big.clone();
        *tail.last_mut().unwrap() = 8;
        let mut middle = big.clone();
        middle[SAMPLE as usize + 10] = 8;
        assert_ne!(print(&big), print(&tail));
        assert_eq!(print(&big), print(&middle), "middle bytes are not sampled");
        assert_ne!(print(b"short"), print(b"shorT"));
        assert_eq!(print(b""), print(b""));
    }
}
