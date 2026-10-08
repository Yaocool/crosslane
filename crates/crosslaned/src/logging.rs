//! Tracing initialization with level-, date-, and size-partitioned log files.

mod trace_log;

pub(crate) use trace_log::scan_trace_logs;

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use crosslane_core::config::LogConfig;
use tracing::{Level, Metadata};
use tracing_subscriber::fmt::writer::{MakeWriter, MakeWriterExt};
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

type FilterHandle = tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;

static FILTER_HANDLE: OnceLock<FilterHandle> = OnceLock::new();
static LOG_RUNTIME: OnceLock<RuntimeLogConfig> = OnceLock::new();
static DROPPED_LOG_RECORDS: AtomicU64 = AtomicU64::new(0);

struct RuntimeLogConfig {
    json: Arc<AtomicBool>,
    file: ReconfigurableFileWriter,
}

pub fn init(config: &LogConfig, default_file_path: PathBuf) -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));
    let (filter, filter_handle) = tracing_subscriber::reload::Layer::new(filter);
    let file = ReconfigurableFileWriter::new(config, default_file_path)?;
    let buffered_file = BufferedMakeWriter::file(file.clone())?;
    let buffered_stdout = BufferedMakeWriter::stdout()?;
    let json = Arc::new(AtomicBool::new(config.format != "pretty"));
    let pretty_switch = Arc::clone(&json);
    let json_switch = Arc::clone(&json);
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .pretty()
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_writer(buffered_stdout.clone().and(buffered_file.clone()))
                .with_filter(tracing_subscriber::filter::filter_fn(move |_| {
                    !pretty_switch.load(Ordering::Acquire)
                })),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true)
                .with_file(true)
                .with_line_number(true)
                .with_current_span(true)
                .with_span_list(true)
                .with_writer(buffered_stdout.and(buffered_file))
                .with_filter(tracing_subscriber::filter::filter_fn(move |_| {
                    json_switch.load(Ordering::Acquire)
                })),
        )
        .try_init()
        .context("initialize tracing")?;
    let _result = FILTER_HANDLE.set(filter_handle);
    let _result = LOG_RUNTIME.set(RuntimeLogConfig { json, file });
    Ok(())
}

pub fn reload_level(level: &str) -> Result<()> {
    let handle: &FilterHandle = FILTER_HANDLE
        .get()
        .ok_or_else(|| anyhow::anyhow!("tracing filter reload handle is unavailable"))?;
    FilterHandle::reload(handle, EnvFilter::new(level))
        .map_err(|error| anyhow::anyhow!("reload tracing filter: {error}"))
}

/// Atomically switches level, formatter, and the partitioned file destination.
pub fn reload_config(config: &LogConfig) -> Result<()> {
    reload_level(&config.level)?;
    let runtime: &RuntimeLogConfig = LOG_RUNTIME
        .get()
        .ok_or_else(|| anyhow::anyhow!("runtime log configuration is unavailable"))?;
    runtime.file.reconfigure(config)?;
    runtime
        .json
        .store(config.format != "pretty", Ordering::Release);
    Ok(())
}

struct BufferedRecord {
    level: Level,
    bytes: Vec<u8>,
}

#[derive(Clone)]
struct BufferedMakeWriter {
    sender: std::sync::mpsc::SyncSender<BufferedRecord>,
}

impl BufferedMakeWriter {
    fn file(writer: ReconfigurableFileWriter) -> Result<Self> {
        Self::spawn("crosslane-log-file", move |records| {
            writer.write_batch(records)
        })
    }

    fn stdout() -> Result<Self> {
        let mut stdout = std::io::stdout();
        Self::spawn("crosslane-log-stdout", move |records| {
            for record in records {
                stdout.write_all(&record.bytes)?;
            }
            stdout.flush()
        })
    }

    fn spawn<F>(thread_name: &str, mut write_batch: F) -> Result<Self>
    where
        F: FnMut(&[BufferedRecord]) -> io::Result<()> + Send + 'static,
    {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<BufferedRecord>(4_096);
        std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || loop {
                let first = match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(record) => record,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let mut size = first.bytes.len();
                let mut batch = vec![first];
                while size < 256 * 1024 {
                    match receiver.try_recv() {
                        Ok(record) => {
                            size = size.saturating_add(record.bytes.len());
                            batch.push(record);
                        }
                        Err(_) => break,
                    }
                }
                let _result = write_batch(&batch);
            })
            .context("start buffered log writer")?;
        Ok(Self { sender })
    }

    fn writer(&self, level: Level) -> BufferedWriter {
        BufferedWriter {
            sender: self.sender.clone(),
            level,
        }
    }
}

impl<'a> MakeWriter<'a> for BufferedMakeWriter {
    type Writer = BufferedWriter;

    fn make_writer(&'a self) -> Self::Writer {
        self.writer(Level::INFO)
    }

    fn make_writer_for(&'a self, metadata: &Metadata<'_>) -> Self::Writer {
        self.writer(*metadata.level())
    }
}

struct BufferedWriter {
    sender: std::sync::mpsc::SyncSender<BufferedRecord>,
    level: Level,
}

impl Write for BufferedWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let record = BufferedRecord {
            level: self.level,
            bytes: buffer.to_vec(),
        };
        match self.sender.try_send(record) {
            Ok(()) => Ok(buffer.len()),
            Err(std::sync::mpsc::TrySendError::Full(_)) => {
                DROPPED_LOG_RECORDS.fetch_add(1, Ordering::Relaxed);
                Ok(buffer.len())
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "log writer stopped",
            )),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone)]
struct ReconfigurableFileWriter {
    state: Arc<Mutex<DatedLogFiles>>,
    default_path: Arc<PathBuf>,
}

impl ReconfigurableFileWriter {
    fn new(config: &LogConfig, default_path: PathBuf) -> Result<Self> {
        let path = configured_log_path(config, &default_path);
        Ok(Self {
            state: Arc::new(Mutex::new(DatedLogFiles::new(
                path,
                maximum_bytes(config),
                config.retention_days,
                config.max_files_per_day,
            )?)),
            default_path: Arc::new(default_path),
        })
    }

    fn reconfigure(&self, config: &LogConfig) -> Result<()> {
        let next = DatedLogFiles::new(
            configured_log_path(config, &self.default_path),
            maximum_bytes(config),
            config.retention_days,
            config.max_files_per_day,
        )?;
        *self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = next;
        Ok(())
    }

    fn write_batch(&self, records: &[BufferedRecord]) -> io::Result<()> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let day = unix_days_now();
        for record in records {
            state.write_at(&record.level, &record.bytes, day)?;
        }
        state.flush()
    }

    #[cfg(test)]
    fn write_at(&self, level: Level, buffer: &[u8], day: i64) -> io::Result<()> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .write_at(&level, buffer, day)
    }
}

pub(crate) fn configured_log_path(config: &LogConfig, default_path: &Path) -> PathBuf {
    if config.file_path.trim().is_empty() {
        default_path.to_path_buf()
    } else {
        PathBuf::from(&config.file_path)
    }
}

#[derive(Debug)]
pub(crate) struct DiscoveredLogFile {
    pub path: PathBuf,
    pub level: String,
    pub date: String,
    pub size_bytes: u64,
    pub modified_at: Option<i64>,
}

pub(crate) fn discover_log_files(base_path: &Path) -> io::Result<Vec<DiscoveredLogFile>> {
    let directory = base_path.parent().unwrap_or_else(|| Path::new("."));
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(date) = matching_log_date(base_path, &path) else {
            continue;
        };
        let Some(level) = matching_log_level(base_path, &path, &date) else {
            continue;
        };
        let metadata = match entry.metadata() {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => continue,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let modified_at = metadata
            .modified()
            .ok()
            .and_then(|modified| modified.duration_since(UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok());
        files.push(DiscoveredLogFile {
            path,
            level: level.into(),
            date,
            size_bytes: metadata.len(),
            modified_at,
        });
    }
    files.sort_by(|left, right| {
        right
            .modified_at
            .cmp(&left.modified_at)
            .then_with(|| right.date.cmp(&left.date))
            .then_with(|| left.level.cmp(&right.level))
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(files)
}

fn matching_log_level(base: &Path, candidate: &Path, date: &str) -> Option<&'static str> {
    let stem = base.file_stem()?.to_str()?;
    let name = candidate.file_name()?.to_str()?;
    let suffix = name.strip_prefix(&format!("{stem}-{date}-"))?;
    ["trace", "debug", "info", "warn", "error"]
        .into_iter()
        .find(|level| suffix.starts_with(level))
}

fn maximum_bytes(config: &LogConfig) -> u64 {
    config.max_file_size_mb.max(1).saturating_mul(1024 * 1024)
}

struct DatedLogFiles {
    base_path: PathBuf,
    maximum: u64,
    retention_days: u64,
    max_files_per_day: usize,
    current_day: Option<i64>,
    files: BTreeMap<&'static str, DatedRotationState>,
}

impl DatedLogFiles {
    fn new(
        base_path: PathBuf,
        maximum: u64,
        retention_days: u64,
        max_files_per_day: u64,
    ) -> Result<Self> {
        if let Some(parent) = base_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create log directory {}", parent.display()))?;
        }
        let output = Self {
            base_path,
            maximum: maximum.max(1),
            retention_days: retention_days.max(1),
            max_files_per_day: usize::try_from(max_files_per_day.max(1)).unwrap_or(usize::MAX),
            current_day: None,
            files: BTreeMap::new(),
        };
        output.cleanup(unix_days_now())?;
        Ok(output)
    }

    fn write_at(&mut self, level: &Level, buffer: &[u8], day: i64) -> io::Result<()> {
        if self.current_day != Some(day) {
            self.flush()?;
            self.files.clear();
            self.cleanup(day)?;
            self.current_day = Some(day);
        }
        let name = level_name(level);
        let state = match self.files.entry(name) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                let date = date_string(day);
                entry.insert(DatedRotationState::open(
                    self.base_path.clone(),
                    date,
                    name,
                    self.maximum,
                    self.max_files_per_day,
                )?)
            }
        };
        state.write_all(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        for file in self.files.values_mut() {
            file.flush()?;
        }
        Ok(())
    }

    fn cleanup(&self, current_day: i64) -> io::Result<()> {
        let Some(parent) = self.base_path.parent() else {
            return Ok(());
        };
        let keep = i64::try_from(self.retention_days.saturating_sub(1)).unwrap_or(i64::MAX);
        let cutoff = date_string(current_day.saturating_sub(keep));
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let path = entry.path();
            if matching_log_date(&self.base_path, &path).is_some_and(|date| date < cutoff) {
                std::fs::remove_file(path)?;
            }
        }
        Ok(())
    }
}

struct DatedRotationState {
    base_path: PathBuf,
    date: String,
    level: &'static str,
    maximum: u64,
    max_files: usize,
    index: usize,
    size: u64,
    file: File,
    discarding: bool,
}

impl DatedRotationState {
    fn open(
        base_path: PathBuf,
        date: String,
        level: &'static str,
        maximum: u64,
        max_files: usize,
    ) -> io::Result<Self> {
        let mut index = 0;
        loop {
            let path = dated_path(&base_path, &date, level, index);
            let file = open_append(&path)?;
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if size < maximum || size == 0 {
                return Ok(Self {
                    base_path,
                    date,
                    level,
                    maximum,
                    max_files,
                    index,
                    size,
                    file,
                    discarding: false,
                });
            }
            if index.saturating_add(1) >= max_files {
                return Ok(Self {
                    base_path,
                    date,
                    level,
                    maximum,
                    max_files,
                    index,
                    size,
                    file,
                    discarding: true,
                });
            }
            index = index.saturating_add(1);
        }
    }

    fn prepare(&mut self, incoming: u64) -> io::Result<bool> {
        if self.discarding {
            return Ok(false);
        }
        if self.size == 0 || self.size.saturating_add(incoming) <= self.maximum {
            return Ok(true);
        }
        self.file.flush()?;
        self.file.sync_data()?;
        loop {
            if self.index.saturating_add(1) >= self.max_files {
                self.discarding = true;
                return Ok(false);
            }
            self.index = self.index.saturating_add(1);
            let path = dated_path(&self.base_path, &self.date, self.level, self.index);
            let file = open_append(&path)?;
            let size = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
            if size < self.maximum || size == 0 {
                self.file = file;
                self.size = size;
                return Ok(true);
            }
        }
    }
}

impl Write for DatedRotationState {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if !self.prepare(buffer.len() as u64)? {
            return Ok(buffer.len());
        }
        let written = self.file.write(buffer)?;
        self.size = self.size.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

fn level_name(level: &Level) -> &'static str {
    if *level == Level::ERROR {
        "error"
    } else if *level == Level::WARN {
        "warn"
    } else if *level == Level::INFO {
        "info"
    } else if *level == Level::DEBUG {
        "debug"
    } else {
        "trace"
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn dated_path(base: &Path, date: &str, level: &str, index: usize) -> PathBuf {
    let parent = base.parent().unwrap_or_else(|| Path::new("."));
    let stem = base
        .file_stem()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("crosslaned");
    let shard = if index == 0 {
        String::new()
    } else {
        format!(".{index}")
    };
    let extension = base
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    parent.join(format!("{stem}-{date}-{level}{shard}{extension}"))
}

fn matching_log_date(base: &Path, candidate: &Path) -> Option<String> {
    let stem = base.file_stem()?.to_str()?;
    let name = candidate.file_name()?.to_str()?;
    let rest = name.strip_prefix(&format!("{stem}-"))?;
    let date = rest.get(..10)?;
    if !valid_date_shape(date) {
        return None;
    }
    let suffix = rest.get(10..)?;
    let extension = base
        .extension()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty());
    if !["trace", "debug", "info", "warn", "error"]
        .iter()
        .any(|level| {
            suffix
                .strip_prefix(&format!("-{level}"))
                .is_some_and(|rest| valid_shard_suffix(rest, extension))
        })
    {
        return None;
    }
    Some(date.to_owned())
}

fn valid_shard_suffix(value: &str, extension: Option<&str>) -> bool {
    let shard = match extension {
        Some(extension) => match value.strip_suffix(&format!(".{extension}")) {
            Some(shard) => shard,
            None => return false,
        },
        None => value,
    };
    shard.is_empty()
        || shard.strip_prefix('.').is_some_and(|index| {
            !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit())
        })
}

fn valid_date_shape(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes()[4] == b'-'
        && value.as_bytes()[7] == b'-'
        && value
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn unix_days_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| (duration.as_secs() / 86_400) as i64)
        .unwrap_or(0)
}

fn date_string(days: i64) -> String {
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

// Howard Hinnant's civil calendar conversion, epoch adjusted to Unix day zero.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_log_queue_drops_without_blocking_the_caller() {
        let (sender, _receiver) = std::sync::mpsc::sync_channel(1);
        sender
            .send(BufferedRecord {
                level: Level::INFO,
                bytes: b"first".to_vec(),
            })
            .expect("fill queue");
        let mut writer = BufferedWriter {
            sender,
            level: Level::INFO,
        };
        let dropped_before = DROPPED_LOG_RECORDS.load(Ordering::Relaxed);

        assert_eq!(writer.write(b"second").expect("drop succeeds"), 6);
        assert_eq!(
            DROPPED_LOG_RECORDS.load(Ordering::Relaxed),
            dropped_before + 1
        );
    }

    #[test]
    fn splits_levels_and_rotates_each_day_independently() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let day = 20_000;
        let date = date_string(day);
        let mut files = DatedLogFiles::new(base.clone(), 8, 3, 3).expect("writer");
        for _ in 0..3 {
            files.write_at(&Level::INFO, b"12345\n", day).expect("info");
        }
        files
            .write_at(&Level::WARN, b"warning\n", day)
            .expect("warn");
        files
            .write_at(&Level::ERROR, b"failure\n", day)
            .expect("error");
        files.flush().expect("flush");

        assert!(dated_path(&base, &date, "info", 0).exists());
        assert!(dated_path(&base, &date, "info", 1).exists());
        assert!(dated_path(&base, &date, "info", 2).exists());
        assert!(dated_path(&base, &date, "warn", 0).exists());
        assert!(dated_path(&base, &date, "error", 0).exists());
        assert!(!dated_path(&base, &date, "debug", 0).exists());
        let info_contents = (0..3)
            .map(|shard| {
                std::fs::read_to_string(dated_path(&base, &date, "info", shard))
                    .expect("read info shard")
            })
            .collect::<String>();
        let warn_contents =
            std::fs::read_to_string(dated_path(&base, &date, "warn", 0)).expect("read warn");
        let error_contents =
            std::fs::read_to_string(dated_path(&base, &date, "error", 0)).expect("read error");
        assert_eq!(info_contents, "12345\n12345\n12345\n");
        assert!(!info_contents.contains("warning"));
        assert!(!info_contents.contains("failure"));
        assert_eq!(warn_contents, "warning\n");
        assert_eq!(error_contents, "failure\n");
    }

    #[test]
    fn cleanup_keeps_the_current_day_and_two_previous_days() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let current = unix_days_now();
        for day in (current - 3)..=current {
            let path = dated_path(&base, &date_string(day), "info", 0);
            std::fs::write(path, b"log").expect("seed log");
        }
        std::fs::write(directory.path().join("unrelated.log"), b"keep").expect("unrelated");

        let files = DatedLogFiles::new(base.clone(), 100, 3, 3).expect("writer");
        files.cleanup(current).expect("cleanup");

        assert!(!dated_path(&base, &date_string(current - 3), "info", 0).exists());
        for day in (current - 2)..=current {
            assert!(dated_path(&base, &date_string(day), "info", 0).exists());
        }
        assert!(directory.path().join("unrelated.log").exists());
        let similar = directory.path().join(format!(
            "crosslaned-{}-info-manual.log",
            date_string(current - 3)
        ));
        std::fs::write(&similar, b"keep").expect("similar unrelated log");
        files.cleanup(current).expect("second cleanup");
        assert!(similar.exists());
    }

    #[test]
    fn discovers_only_partitioned_log_files_with_metadata() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let info = dated_path(&base, "2026-08-23", "info", 0);
        let error = dated_path(&base, "2026-08-23", "error", 1);
        std::fs::write(&info, b"info\n").expect("info log");
        std::fs::write(&error, b"failure\n").expect("error log");
        std::fs::write(directory.path().join("unrelated.log"), b"ignore").expect("unrelated file");

        let files = discover_log_files(&base).expect("discover logs");
        assert_eq!(files.len(), 2);
        assert!(files
            .iter()
            .any(|file| file.path == info && file.level == "info" && file.size_bytes == 5));
        assert!(files
            .iter()
            .any(|file| file.path == error && file.level == "error" && file.size_bytes == 8));
        assert!(files.iter().all(|file| file.date == "2026-08-23"));
    }

    #[test]
    fn runtime_writer_switches_base_paths_without_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let first = directory.path().join("first.log");
        let second = directory.path().join("second.log");
        let day = 20_000;
        let date = date_string(day);
        let mut config = LogConfig {
            file_path: first.display().to_string(),
            ..LogConfig::default()
        };
        let writer = ReconfigurableFileWriter::new(&config, first.clone()).expect("writer");
        writer
            .write_at(Level::INFO, b"first\n", day)
            .expect("first write");
        config.file_path = second.display().to_string();
        writer.reconfigure(&config).expect("reconfigure");
        writer
            .write_at(Level::INFO, b"second\n", day)
            .expect("second write");

        assert!(dated_path(&first, &date, "info", 0).exists());
        assert!(dated_path(&second, &date, "info", 0).exists());
    }

    #[test]
    fn stops_creating_file_shards_at_the_daily_limit() {
        let directory = tempfile::tempdir().expect("tempdir");
        let base = directory.path().join("crosslaned.log");
        let day = 20_000;
        let date = date_string(day);
        let mut files = DatedLogFiles::new(base.clone(), 8, 3, 2).expect("writer");
        for _ in 0..10 {
            files
                .write_at(&Level::INFO, b"12345\n", day)
                .expect("write");
        }
        files.flush().expect("flush");

        assert!(dated_path(&base, &date, "info", 0).exists());
        assert!(dated_path(&base, &date, "info", 1).exists());
        assert!(!dated_path(&base, &date, "info", 2).exists());
    }
}
