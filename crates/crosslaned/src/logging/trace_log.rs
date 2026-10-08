use std::fs::File;
use std::io::{self, BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::discover_log_files;

const MAX_TRACE_FILES: usize = 256;
const MAX_TRACE_SCAN_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TRACE_MATCHES: usize = 10_000;

#[derive(Debug)]
pub(crate) struct TraceLogRecord {
    pub path: PathBuf,
    pub level: String,
    pub date: String,
    pub line: u64,
    pub record: Value,
}

#[derive(Debug)]
pub(crate) struct TraceLogScan {
    pub entries: Vec<TraceLogRecord>,
    pub files_scanned: usize,
    pub bytes_scanned: u64,
    pub matched_records: usize,
    pub truncated: bool,
}

pub(crate) fn scan_trace_logs(
    base_path: &Path,
    trace_id: &str,
    level: Option<&str>,
    tail: usize,
) -> io::Result<TraceLogScan> {
    let mut entries = Vec::new();
    let mut files_scanned = 0usize;
    let mut bytes_scanned = 0u64;
    let mut matched_records = 0usize;
    let mut truncated = false;

    for file in discover_log_files(base_path)?
        .into_iter()
        .filter(|file| level.is_none_or(|level| file.level == level))
    {
        if files_scanned >= MAX_TRACE_FILES || bytes_scanned >= MAX_TRACE_SCAN_BYTES {
            truncated = true;
            break;
        }
        let remaining = MAX_TRACE_SCAN_BYTES.saturating_sub(bytes_scanned);
        if remaining == 0 {
            truncated = true;
            break;
        }
        let input = File::open(&file.path)?;
        // Use UFCS here instead of `input.take(...)`. Some rust-analyzer
        // versions incorrectly resolve the method as `Iterator::take` for
        // `std::fs::File`, even though `File` implements `std::io::Read`.
        let mut reader = BufReader::new(std::io::Read::take(input, remaining));
        let mut buffer = String::new();
        let mut line_number = 0u64;
        files_scanned = files_scanned.saturating_add(1);
        loop {
            buffer.clear();
            let read = reader.read_line(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes_scanned = bytes_scanned.saturating_add(read as u64);
            line_number = line_number.saturating_add(1);
            let line = buffer.trim_end_matches(['\r', '\n']);
            let parsed = serde_json::from_str::<Value>(line).ok();
            let matches = parsed
                .as_ref()
                .is_some_and(|record| json_has_trace_id(record, trace_id))
                || parsed.is_none() && line.contains(trace_id);
            if !matches {
                continue;
            }
            matched_records = matched_records.saturating_add(1);
            if entries.len() >= MAX_TRACE_MATCHES {
                truncated = true;
                break;
            }
            entries.push(TraceLogRecord {
                path: file.path.clone(),
                level: file.level.clone(),
                date: file.date.clone(),
                line: line_number,
                record: parsed.unwrap_or_else(|| Value::String(line.to_owned())),
            });
        }
        if entries.len() >= MAX_TRACE_MATCHES {
            break;
        }
        if bytes_scanned >= MAX_TRACE_SCAN_BYTES && file.size_bytes > remaining {
            truncated = true;
            break;
        }
    }

    entries.sort_by(|left, right| {
        record_timestamp(&left.record)
            .unwrap_or(&left.date)
            .cmp(record_timestamp(&right.record).unwrap_or(&right.date))
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.line.cmp(&right.line))
    });
    let tail = tail.clamp(1, 1_000);
    if entries.len() > tail {
        let remove = entries.len() - tail;
        entries.drain(..remove);
        truncated = true;
    }

    Ok(TraceLogScan {
        entries,
        files_scanned,
        bytes_scanned,
        matched_records,
        truncated,
    })
}

fn json_has_trace_id(value: &Value, trace_id: &str) -> bool {
    match value {
        Value::Object(object) => object.iter().any(|(key, value)| {
            key == "trace_id" && value.as_str() == Some(trace_id)
                || json_has_trace_id(value, trace_id)
        }),
        Value::Array(values) => values
            .iter()
            .any(|value| json_has_trace_id(value, trace_id)),
        _ => false,
    }
}

fn record_timestamp(record: &Value) -> Option<&str> {
    record.get("timestamp").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_scan_combines_exact_severity_shards_in_timestamp_order() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let trace_id = "trace_0123456789abcdef0123456789abcdef";
        let info = directory.path().join("crosslaned-2026-08-31-info.log");
        let warn = directory.path().join("crosslaned-2026-08-31-warn.log");
        let error = directory.path().join("crosslaned-2026-08-31-error.log");
        std::fs::write(
            &info,
            format!(
                "{{\"timestamp\":\"2026-08-31T01:00:00Z\",\"level\":\"INFO\",\"fields\":{{\"message\":\"start\",\"trace_id\":\"{trace_id}\"}}}}\n"
            ),
        )
        .expect("info");
        std::fs::write(
            &warn,
            "{\"timestamp\":\"2026-08-31T01:00:01Z\",\"level\":\"WARN\",\"fields\":{\"message\":\"other\",\"trace_id\":\"trace_other\"}}\n",
        )
        .expect("warn");
        std::fs::write(
            &error,
            format!(
                "{{\"timestamp\":\"2026-08-31T01:00:02Z\",\"level\":\"ERROR\",\"span\":{{\"trace_id\":\"{trace_id}\"}},\"fields\":{{\"message\":\"failed\"}}}}\n"
            ),
        )
        .expect("error");

        let scan = scan_trace_logs(&base, trace_id, None, 100).expect("scan");

        assert_eq!(scan.files_scanned, 3);
        assert_eq!(scan.matched_records, 2);
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.entries[0].level, "info");
        assert_eq!(scan.entries[1].level, "error");
        assert!(!scan.truncated);
    }

    #[test]
    fn trace_scan_honors_level_and_tail_limits() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let trace_id = "trace_0123456789abcdef0123456789abcdef";
        let info = directory.path().join("crosslaned-2026-08-31-info.log");
        let records = (0..3)
            .map(|index| {
                format!(
                    "{{\"timestamp\":\"2026-08-31T01:00:0{index}Z\",\"fields\":{{\"trace_id\":\"{trace_id}\",\"message\":\"event {index}\"}}}}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(info, format!("{records}\n")).expect("logs");

        let scan = scan_trace_logs(&base, trace_id, Some("info"), 2).expect("scan");

        assert_eq!(scan.matched_records, 3);
        assert_eq!(scan.entries.len(), 2);
        assert_eq!(scan.entries[0].record["fields"]["message"], "event 1");
        assert_eq!(scan.entries[1].record["fields"]["message"], "event 2");
        assert!(scan.truncated);
    }
}
