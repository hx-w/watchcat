use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Event {
    pub timestamp: DateTime<Utc>,
    pub kind: String,
    pub pid: Option<i32>,
    pub app: String,
    pub rule: Option<String>,
    pub button: Option<String>,
    pub detail: String,
}

pub fn path(state_socket: &Path) -> PathBuf {
    state_socket.with_file_name("dialog-events.jsonl")
}

pub fn read(path: &Path, clicks_only: bool, limit: usize) -> Result<Vec<Event>> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut entries = VecDeque::new();
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        // An append in flight is not a corrupt record. Read it on the next call.
        if !line.ends_with('\n') {
            break;
        }
        let event: Event = serde_json::from_str(&line)
            .with_context(|| format!("invalid dialog event in {}", path.display()))?;
        if clicks_only && !event.kind.starts_with("click.") {
            continue;
        }
        entries.push_back(event);
        if entries.len() > limit {
            entries.pop_front();
        }
    }
    Ok(entries.into_iter().collect())
}

pub(super) struct Log {
    path: PathBuf,
    count: usize,
    retention: usize,
    repair_required: bool,
}

impl Log {
    pub fn new(path: PathBuf, retention: usize) -> Result<Self> {
        // A crash can leave a partial last record. Remove that incomplete tail
        // before the next append, preserving all complete audit records.
        match fs::read(&path) {
            Ok(bytes) if !bytes.is_empty() && !bytes.ends_with(b"\n") => {
                let end = bytes.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                OpenOptions::new()
                    .write(true)
                    .open(&path)?
                    .set_len(end as u64)?;
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let count = read(&path, false, usize::MAX)?.len();
        Ok(Self {
            path,
            count,
            retention,
            repair_required: false,
        })
    }

    pub fn append(&mut self, event: &Event) -> Result<()> {
        self.append_with(event, |file, bytes| file.write_all(bytes))
    }

    fn append_with(
        &mut self,
        event: &Event,
        write: impl FnOnce(&mut fs::File, &[u8]) -> std::io::Result<()>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.repair_required,
            "dialog log needs repair; restart the service"
        );
        fs::create_dir_all(self.path.parent().context("dialog log has no parent")?)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&self.path)?;
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        let complete_length = file.metadata()?.len();
        if let Err(error) = write(&mut file, &bytes) {
            if let Err(repair_error) = file.set_len(complete_length) {
                self.repair_required = true;
                return Err(error).context(format!("cannot restore dialog log: {repair_error}"));
            }
            return Err(error.into());
        }
        self.count += 1;
        if self.count > self.retention.saturating_mul(2) {
            let entries = read(&self.path, false, self.retention)?;
            let mut bytes = Vec::new();
            for entry in &entries {
                serde_json::to_writer(&mut bytes, entry)?;
                bytes.push(b'\n');
            }
            crate::config::atomic_write(&self.path, &bytes)?;
            self.count = entries.len();
        }
        Ok(())
    }
}

// The event log is written by the macOS/Linux daemon; Windows has no service.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn release_regression_partial_append_does_not_corrupt_future_records() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("events.jsonl");
        let mut log = Log::new(path.clone(), 10).unwrap();
        let event = Event {
            timestamp: Utc::now(),
            kind: "click.attempt".into(),
            pid: Some(123),
            app: "Helper".into(),
            rule: Some("helper".into()),
            button: Some("Continue".into()),
            detail: "test fixture".into(),
        };
        log.append(&event).unwrap();
        let complete_length = fs::metadata(&path).unwrap().len();
        assert!(
            log.append_with(&event, |file, bytes| {
                file.write_all(&bytes[..bytes.len() / 2])?;
                Err(std::io::Error::other("injected partial append failure"))
            })
            .is_err()
        );
        assert_eq!(fs::metadata(&path).unwrap().len(), complete_length);
        log.append(&event).unwrap();
        assert_eq!(read(&path, false, 10).unwrap().len(), 2);
        drop(log);
        let mut resumed = Log::new(path.clone(), 10).unwrap();
        resumed.append(&event).unwrap();
        assert_eq!(read(&path, false, 10).unwrap().len(), 3);
    }

    #[test]
    fn persistent_logs_filter_clicks_bound_retention_and_ignore_inflight_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut log = Log::new(path.clone(), 3).unwrap();
        for n in 0..7 {
            log.append(&Event {
                timestamp: Utc::now(),
                kind: if n % 2 == 0 {
                    "click.sent"
                } else {
                    "process.observed"
                }
                .into(),
                pid: Some(n),
                app: "Helper".into(),
                rule: None,
                button: None,
                detail: String::new(),
            })
            .unwrap();
        }
        assert_eq!(read(&path, false, 99).unwrap().len(), 3);
        let clicks = read(&path, true, 1).unwrap();
        assert_eq!(clicks[0].pid, Some(6));
        drop(log);
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"timestamp\":")
            .unwrap();
        assert_eq!(read(&path, false, 99).unwrap().len(), 3);
        let mut resumed = Log::new(path.clone(), 3).unwrap();
        resumed.append(&clicks[0]).unwrap();
        assert_eq!(read(&path, false, 99).unwrap().len(), 4);
    }
}
