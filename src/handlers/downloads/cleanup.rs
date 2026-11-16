//! Recorded outcomes from the bounded administrative source-cleanup tool.
//! Display only: receipts never authorize filesystem or client operations.
use serde::Deserialize;
use std::collections::BTreeMap;

pub(super) struct RetirementView {
    pub record: crate::models::seed_policy::Retirement,
    pub cleanup: String,
}
#[derive(Default)]
pub(super) struct History {
    entries: BTreeMap<String, Entry>,
    pub error: String,
}
#[derive(Deserialize)]
struct Receipt {
    format_version: u32,
    updated_at: String,
    entries: Vec<Entry>,
}
#[derive(Deserialize)]
struct Entry {
    client_id: i64,
    hash: String,
    original_path: String,
    size: u64,
    status: String,
}
impl History {
    pub fn for_source(&self, id: i64, hash: &str) -> String {
        let entries: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.client_id == id && e.hash.eq_ignore_ascii_case(hash))
            .collect();
        if entries.is_empty() {
            return "Files retained · cleanup not recorded".into();
        }
        if entries
            .iter()
            .any(|e| matches!(e.status.as_str(), "planned" | "moving" | "restoring"))
        {
            return "Cleanup planned or in progress · files remain recoverable".into();
        }
        let n = entries.iter().filter(|e| e.status == "quarantined").count();
        let restored = entries.iter().filter(|e| e.status == "restored").count();
        let absent = entries.iter().filter(|e| e.status == "absent").count();
        let held = entries.len() - n - restored - absent;
        if held > 0 {
            format!("{n} quarantined · {held} held for review")
        } else if restored > 0 {
            format!("{restored} restored · source stays blocked")
        } else if n > 0 {
            format!("{n} files quarantined · recoverable")
        } else {
            "Already absent · no files moved".into()
        }
    }
    pub fn summary(&self) -> String {
        let entries: Vec<_> = self
            .entries
            .values()
            .filter(|e| e.status == "quarantined")
            .collect();
        if entries.is_empty() {
            return "Retained files stay separate from job removal and source blocking.".into();
        }
        let bytes = entries.iter().fold(0u64, |n, e| n.saturating_add(e.size));
        format!(
            "Recorded cleanup: {} {} · {} logical size · recoverable quarantine. No automatic expiry; no disk-space saving is claimed.",
            entries.len(),
            if entries.len() == 1 { "file" } else { "files" },
            crate::services::recycle::human_bytes(bytes)
        )
    }
}
pub(super) async fn load() -> History {
    let root = crate::services::paths::data_dir().join("source-cleanup");
    tokio::task::spawn_blocking(move || {
        let mut history = History::default();
        let result = (|| -> Result<(), String> {
            if !root.try_exists().map_err(|e| e.to_string())? {
                return Ok(());
            }
            if root.is_symlink() {
                return Err("Cleanup receipt directory is a symlink".into());
            }
            let mut paths = std::fs::read_dir(&root)
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            paths.sort_by_key(|e| e.file_name());
            let mut receipts = Vec::new();
            if paths
                .iter()
                .filter(|e| e.path().extension().is_some_and(|e| e == "json"))
                .count()
                > 64
            {
                return Err("Too many cleanup receipts for a complete view".into());
            }
            for path in paths {
                let path = path.path();
                if path.extension().is_none_or(|e| e != "json") || path.is_symlink() {
                    continue;
                }
                if std::fs::metadata(&path).map_err(|e| e.to_string())?.len() > 4 * 1024 * 1024 {
                    return Err("Cleanup receipt is too large".into());
                }
                let receipt: Receipt =
                    serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?;
                if receipt.format_version != 1 || receipt.entries.len() > 4096 {
                    return Err("Unsupported cleanup receipt".into());
                }
                receipts.push(receipt);
            }
            receipts.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
            for receipt in receipts {
                for entry in receipt.entries {
                    if crate::services::download_client::normalize_info_hash(&entry.hash).is_none()
                    {
                        return Err("Invalid cleanup source identity".into());
                    }
                    history.entries.insert(entry.original_path.clone(), entry);
                }
            }
            Ok(())
        })();
        if result.is_err() {
            history.entries.clear();
            history.error =
                "Cleanup history is unavailable. File cleanup has not been assumed complete."
                    .into();
        }
        history
    })
    .await
    .unwrap_or_else(|_| History {
        error: "Cleanup history is unavailable.".into(),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(path: &str, status: &str) -> Entry {
        Entry {
            client_id: 1,
            hash: "a".repeat(40),
            original_path: path.into(),
            size: 1024,
            status: status.into(),
        }
    }
    #[test]
    fn receipts_report_recovery_and_holds_without_claiming_disk_savings() {
        let mut h = History::default();
        h.entries
            .insert("/one".into(), entry("/one", "quarantined"));
        h.entries.insert("/two".into(), entry("/two", "held"));
        assert_eq!(
            h.for_source(1, &"a".repeat(40)),
            "1 quarantined · 1 held for review"
        );
        assert!(h.summary().contains("1 file"));
        assert!(h.summary().contains("no disk-space saving"));
        h.entries.insert("/one".into(), entry("/one", "restored"));
        assert!(!h.summary().contains("1 file"));
        assert!(h.for_source(9, &"a".repeat(40)).contains("not recorded"));
    }
    #[test]
    fn interrupted_cleanup_is_never_reported_complete() {
        let mut h = History::default();
        for status in ["planned", "moving", "restoring"] {
            h.entries.insert("/one".into(), entry("/one", status));
            assert!(h.for_source(1, &"a".repeat(40)).contains("in progress"));
        }
    }
}
