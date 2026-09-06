//! Display metadata for scanned files. Filesystem ctime is not a recording date.
use crate::model::Candidate;
use chrono::{DateTime, NaiveDateTime, Utc};

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
    use crate::model::Identity;
    fn candidate(name: &str) -> Candidate {
        Candidate { relative: name.into(), identity: Identity { dev: 0, ino: 0, size: 0, mtime: 0, mtime_ns: 0, ctime: 0, ctime_ns: 0 },
            probe: Some(serde_json::from_value(serde_json::json!({"streams":[],"format":{"tags":{"creation_time":"2025-01-02T03:04:05Z"}}})).unwrap()),
            probe_error: None, selected: true, editing: true, sidecar_for: None }
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
}
