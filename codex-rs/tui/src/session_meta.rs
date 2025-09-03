use codex_protocol::models::ResponseItem;
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionStats {
    pub message_count: Option<u32>,
}

/// Read optional stats (currently only message_count) from a rollout .jsonl file.
/// This scans the file and prefers the latest state line. If not present,
/// `message_count` is derived by counting visible items.
pub(crate) fn read_session_stats(path: &Path, max_bytes: usize) -> SessionStats {
    // Prefer sidecar if present: <rollout>.meta.json
    if let Some(sidecar) = sidecar_path(path)
        && let Ok(bytes) = fs::read(&sidecar)
        && let Ok(v) = serde_json::from_slice::<Value>(&bytes)
    {
        let message_count = v
            .get("message_count")
            .and_then(|n| n.as_u64())
            .map(|n| n as u32);
        return SessionStats { message_count };
    }
    let mut stats = SessionStats::default();
    let Ok(bytes) = fs::read(path) else {
        return stats;
    };
    let slice: &[u8] = if bytes.len() > max_bytes {
        &bytes[bytes.len() - max_bytes..]
    } else {
        &bytes
    };
    let Ok(text) = std::str::from_utf8(slice) else {
        return stats;
    };

    let mut count: u32 = 0;
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(l) {
            if matches!(v.get("record_type").and_then(|x| x.as_str()), Some("state")) {
                if let Some(n) = v.get("message_count").and_then(|n| n.as_u64()) {
                    stats.message_count = Some(n as u32);
                }
                continue;
            }
            // Otherwise, try to parse a ResponseItem to count user/assistant/tool items.
            if serde_json::from_value::<ResponseItem>(v).is_ok() {
                count = count.saturating_add(1);
            }
        } else if l.contains("\"record_type\":\"state\"") {
            // Fallback: very small, permissive extractor for message_count.
            if stats.message_count.is_none()
                && let Some(i) = l.find("\"message_count\":")
            {
                let start = i + "\"message_count\":".len();
                let num: String = l[start..]
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                if let Ok(n) = num.parse::<u32>() {
                    stats.message_count = Some(n);
                }
            }
            continue;
        }
    }
    if stats.message_count.is_none() {
        stats.message_count = Some(count);
    }
    stats
}

// We keep read-only sidecar support to fetch message_count if present.

fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let p = PathBuf::from(path);
    let name = p.file_name()?.to_string_lossy().to_string();
    Some(p.with_file_name(format!("{name}.meta.json")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    #[test]
    fn empty_file_returns_default() {
        let tf = NamedTempFile::new().unwrap();
        let stats = read_session_stats(tf.path(), 128 * 1024);
        assert_eq!(stats.message_count, Some(0));
    }
}
