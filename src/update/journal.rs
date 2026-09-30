use super::{catalog, Core};
use crate::config::read_bounded;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io, path::Path};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Pending {
    pub tag: String,
    pub sha256: String,
    pub from: String,
    pub from_slot: String,
    pub to_slot: String,
    pub channel: String,
    pub boot_id: String,
    pub phase: String,
    pub automatic: bool,
    #[serde(default)]
    pub operation_id: String,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Journal {
    pub pending: Option<Pending>,
    pub target: String,
    pub last_window: String,
    pub last_result: String,
    pub suspended: String,
    pub blocked: BTreeMap<String, String>,
}
pub(super) fn other_slot(slot: &str) -> &'static str {
    match slot {
        "A" => "B",
        "B" => "A",
        _ => "",
    }
}

impl Journal {
    fn read_document(path: &Path) -> io::Result<Self> {
        serde_json::from_slice(&read_bounded(path, 1 << 20)?).map_err(io::Error::other)
    }
    pub fn commit(
        &mut self,
        path: &Path,
        change: impl FnOnce(&mut Self),
        write: impl FnOnce(&Path, &Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut next = self.clone();
        change(&mut next);
        let result = write(path, &next);
        // Rename may be visible despite failed directory fsync. Keep those markers in the cache
        // so a later commit cannot erase them, but never report an uncertain write as durable.
        if result.is_ok() || Self::read_document(path).ok().as_ref() == Some(&next) {
            *self = next;
        }
        result
    }
    pub fn is_empty(&self) -> bool {
        self.pending.is_none()
            && self.target.is_empty()
            && self.last_window.is_empty()
            && self.last_result.is_empty()
            && self.suspended.is_empty()
            && self.blocked.is_empty()
    }
    pub fn load(dir: &Path) -> Result<Self, String> {
        let path = dir.join("state.json");
        let parsed = match Self::read_document(&path) {
            Ok(j) => Some(j),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            // Invalid JSON, oversized data or a non-regular file must be archived and suspended.
            Err(e) if e.kind() == std::io::ErrorKind::Other => None,
            Err(e) => return Err(format!("cannot read update journal: {e}")),
        };
        if let Some(j) = parsed {
            let valid = j
                .pending
                .as_ref()
                .map(|p| {
                    catalog::version(&p.tag).is_some()
                        && catalog::valid_hash(&p.sha256)
                        && !p.boot_id.is_empty()
                        && !other_slot(&p.from_slot).is_empty()
                        && p.to_slot == other_slot(&p.from_slot)
                        && matches!(p.phase.as_str(), "installing" | "installed")
                        && matches!(p.channel.as_str(), "stable" | "test")
                })
                .unwrap_or(true);
            if valid {
                return Ok(j);
            }
        }
        let archive = dir.join(format!(
            "state.json.unreadable-{}",
            crate::common::token().map_err(|e| e.to_string())?
        ));
        let message = format!(
            "the update journal was unreadable (kept as {}); automatic updates suspended",
            archive.file_name().unwrap().to_string_lossy()
        );
        let j = Self {
            suspended: message.clone(),
            last_result: message,
            ..Self::default()
        };
        // Preserve the only evidence if replacing it cannot be made durable.
        fs::rename(&path, &archive).map_err(|e| e.to_string())?;
        if let Err(e) = write_atomic(&path, &j) {
            let _ = fs::rename(&archive, &path);
            return Err(format!("cannot suspend updates durably: {e}"));
        }
        Ok(j)
    }
}

pub(super) fn write_atomic(path: &Path, value: &impl Serialize) -> Result<(), String> {
    let mut raw = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    raw.push(b'\n');
    crate::config::write_atomic(path, &raw).map_err(|e| e.to_string())
}

impl Core {
    pub(super) fn claim_window(&self, window: &str) -> Result<bool, String> {
        // ISO local dates are monotonic claims, including a clock adjustment backwards.
        if window.is_empty() || self.data.lock().unwrap().journal.last_window.as_str() >= window {
            return Ok(false);
        }
        self.commit(|j| j.last_window = window.into())?;
        Ok(true)
    }
    pub(super) fn reconcile_boot(&self, boot: &super::rauc::BootState) -> Result<(), String> {
        let j = self.data.lock().unwrap().journal.clone();
        let mut state = if boot.operation == "idle" {
            "idle"
        } else {
            "installing"
        };
        if let Some(p) = &j.pending {
            let same_boot = p.boot_id == boot.boot_id;
            if boot.operation != "idle" {
                state = "installing";
            } else if same_boot && p.phase == "installed" {
                state = "reboot";
            } else if same_boot {
                state = "uncertain";
                if j.suspended.is_empty() {
                    self.commit(|j| {
                        let msg = format!(
                            "the result of installing {} is unknown; automatic updates suspended",
                            p.tag
                        );
                        j.suspended = msg.clone();
                        j.last_result = msg;
                    })?;
                }
            } else if boot.slot == p.to_slot && self.options.image_version == p.tag {
                match boot.health.as_str() {
                    "good" => self.commit(|j| {
                        j.pending = None;
                        j.suspended.clear();
                        j.blocked.retain(|tag, _| {
                            tag != &p.tag && super::newer(tag, &self.options.image_version)
                        });
                        j.last_result = format!("updated from {} to {}", p.from, p.tag);
                    })?,
                    "stranded" => self.commit(|j| {
                        j.pending = None;
                        let msg = format!(
                            "{} could not be confirmed healthy; automatic updates suspended",
                            p.tag
                        );
                        j.suspended = msg.clone();
                        j.last_result = msg;
                    })?,
                    _ => state = "confirming",
                }
            } else if boot.slot == p.from_slot && p.phase == "installed" {
                self.commit(|j| {
                    j.pending = None;
                    j.blocked.insert(p.tag.clone(), "rolled back".into());
                    j.last_result = format!(
                        "{} did not start correctly, back to {}",
                        p.tag, self.options.image_version
                    );
                })?;
            } else {
                self.commit(|j| {
                    j.pending = None;
                    let msg = format!(
                        "installing {} was interrupted by a restart; automatic updates suspended",
                        p.tag
                    );
                    j.suspended = msg.clone();
                    j.last_result = msg;
                })?;
            }
        }
        self.set(|s| {
            if s.state == "uncertain" && state != "uncertain" {
                s.error.clear();
            }
            // A transient RAUC outage must not turn a confirmed healthy boot into a permanent error.
            if state == "idle" && !s.error.is_empty() && s.state != "uncertain" {
                s.state = "error".into();
            } else {
                s.state = state.into();
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_write_adopts_visible_markers_without_claiming_durability() {
        let dir =
            std::env::temp_dir().join(format!("update-commit-{}", crate::common::token().unwrap()));
        let path = dir.join("state.json");
        let mut cached = Journal::default();
        write_atomic(&path, &cached).unwrap();
        let pending = Pending {
            tag: "v1.2.0".into(),
            sha256: "a".repeat(64),
            from: "v1.1.0".into(),
            from_slot: "A".into(),
            to_slot: "B".into(),
            channel: "stable".into(),
            boot_id: "boot-1".into(),
            phase: "installing".into(),
            automatic: true,
            operation_id: "operation-1".into(),
        };
        // The injected writer performs the real replacement, then models its final fsync error.
        let result = cached.commit(
            &path,
            |next| {
                next.pending = Some(pending.clone());
                next.last_window = "2026-09-30".into();
            },
            |path, next| {
                write_atomic(path, next)?;
                Err("directory fsync failed after rename".into())
            },
        );
        assert_eq!(result.unwrap_err(), "directory fsync failed after rename");
        assert_eq!(cached.pending, Some(pending.clone()));
        assert_eq!(cached.last_window, "2026-09-30");
        assert_eq!(cached, Journal::read_document(&path).unwrap());
        cached
            .commit(
                &path,
                |next| next.last_result = "recovering".into(),
                write_atomic,
            )
            .unwrap();
        assert_eq!(Journal::load(&dir).unwrap().pending, Some(pending));
        assert_eq!(Journal::load(&dir).unwrap().last_window, "2026-09-30");
        let visible = cached.clone();
        assert!(cached
            .commit(
                &path,
                |next| next.last_window = "2026-10-01".into(),
                |_, _| { Err("write failed before rename".into()) }
            )
            .is_err());
        assert_eq!(cached, visible);
        assert_eq!(Journal::read_document(&path).unwrap(), visible);
        fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn durable_roundtrip_and_corruption() {
        let dir = std::env::temp_dir().join(format!(
            "update-journal-{}",
            crate::common::token().unwrap()
        ));
        let path = dir.join("state.json");
        let j = Journal {
            last_window: "2026-09-29".into(),
            ..Journal::default()
        };
        write_atomic(&path, &j).unwrap();
        assert_eq!(Journal::load(&dir).unwrap().last_window, j.last_window);
        fs::write(&path, b"{").unwrap();
        assert!(!Journal::load(&dir).unwrap().suspended.is_empty());
        assert!(!Journal::load(&dir).unwrap().suspended.is_empty());
        assert_eq!(
            fs::read_dir(&dir)
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("unreadable"))
                .count(),
            1
        );
        fs::remove_dir_all(dir).unwrap();
    }
}
