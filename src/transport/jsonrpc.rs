use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{Mutex, broadcast, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use super::AcknowledgementUnknown;

#[derive(Debug, Error)]
#[error("JSON-RPC error {code}: {message}")]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

pub struct JsonRpcClient {
    command: Vec<String>,
    timeout: Duration,
    child: Option<Child>,
    stdin: Option<Arc<Mutex<ChildStdin>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>,
    notifications_tx: broadcast::Sender<Value>,
    notifications_rx: broadcast::Receiver<Value>,
    reader: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
    next_id: AtomicU64,
}

impl JsonRpcClient {
    pub fn new(command: Vec<String>) -> Result<Self> {
        let mut command = command;
        if command.is_empty() {
            bail!("JSON-RPC command cannot be empty");
        }
        command[0] = resolve_executable(&command[0])
            .unwrap_or_else(|| PathBuf::from(&command[0]))
            .to_string_lossy()
            .into_owned();
        let (notifications_tx, notifications_rx) = broadcast::channel(256);
        Ok(Self {
            command,
            timeout: Duration::from_secs(30),
            child: None,
            stdin: None,
            pending: Arc::new(Mutex::new(HashMap::new())),
            notifications_tx,
            notifications_rx,
            reader: None,
            stderr_reader: None,
            next_id: AtomicU64::new(1),
        })
    }

    pub async fn start(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                return Ok(());
            }
            self.reset_process().await;
        }
        let mut command = Command::new(&self.command[0]);
        command
            .args(&self.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start {}", self.command.join(" ")))?;
        let stdin = child
            .stdin
            .take()
            .context("JSON-RPC process has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("JSON-RPC process has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("JSON-RPC process has no stderr")?;
        self.stdin = Some(Arc::new(Mutex::new(stdin)));

        let pending = Arc::clone(&self.pending);
        let notifications = self.notifications_tx.clone();
        self.reader = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let message: Value = match serde_json::from_str(&line) {
                            Ok(message) => message,
                            Err(error) => {
                                warn!(%error, "ignoring non-JSON app-server output");
                                continue;
                            }
                        };
                        if message.get("method").and_then(Value::as_str).is_some() {
                            if notifications.send(message).is_err() {
                                break;
                            }
                        } else if let Some(id) = message.get("id").and_then(Value::as_u64) {
                            if let Some(sender) = pending.lock().await.remove(&id) {
                                let result = if let Some(error) = message.get("error") {
                                    Err(parse_rpc_error(error).into())
                                } else {
                                    Ok(message.get("result").cloned().unwrap_or(Value::Null))
                                };
                                let _ = sender.send(result);
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        warn!(%error, "app-server stdout reader failed");
                        break;
                    }
                }
            }
            let mut pending = pending.lock().await;
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(anyhow!("app-server stdout closed")));
            }
        }));
        self.stderr_reader = Some(tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                debug!(message = line, "app-server");
            }
        }));
        self.child = Some(child);
        Ok(())
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, false).await
    }

    pub async fn request_with_unknown_ack(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_inner(method, params, true).await
    }

    async fn request_inner(
        &mut self,
        method: &str,
        params: Value,
        side_effect_may_have_started: bool,
    ) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().await.insert(id, sender);
        if let Err(error) = self
            .send(&json!({"method": method, "id": id, "params": params}))
            .await
        {
            self.pending.lock().await.remove(&id);
            self.discard_unhealthy_process().await;
            return Err(error);
        }
        match tokio::time::timeout(self.timeout, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => {
                self.discard_unhealthy_process().await;
                if side_effect_may_have_started {
                    Err(AcknowledgementUnknown(format!(
                        "app-server closed while waiting for {method}"
                    ))
                    .into())
                } else {
                    bail!("app-server closed while waiting for {method}")
                }
            }
            Err(_) => {
                self.pending.lock().await.remove(&id);
                self.discard_unhealthy_process().await;
                if side_effect_may_have_started {
                    Err(
                        AcknowledgementUnknown(format!("app-server request timed out: {method}"))
                            .into(),
                    )
                } else {
                    bail!("app-server request timed out: {method}")
                }
            }
        }
    }

    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.send(&json!({"method": method, "params": params}))
            .await
    }

    pub async fn next_notification(&mut self, timeout: Duration) -> Result<Option<Value>> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            match tokio::time::timeout(remaining, self.notifications_rx.recv()).await {
                Ok(Ok(value)) => return Ok(Some(value)),
                Ok(Err(broadcast::error::RecvError::Lagged(skipped))) => {
                    warn!(skipped, "app-server notifications lagged");
                }
                Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return Ok(None),
            }
        }
    }

    pub async fn close(&mut self) -> Result<()> {
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            if child.try_wait()?.is_none() {
                child.start_kill()?;
                let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            }
        }
        self.child.take();
        for task in [self.reader.take(), self.stderr_reader.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        Ok(())
    }

    pub async fn is_running(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(false);
        };
        if child.try_wait()?.is_none() {
            return Ok(true);
        }
        self.reset_process().await;
        Ok(false)
    }

    async fn reset_process(&mut self) {
        self.stdin.take();
        self.child.take();
        for task in [self.reader.take(), self.stderr_reader.take()]
            .into_iter()
            .flatten()
        {
            task.abort();
        }
        let mut pending = self.pending.lock().await;
        for (_, sender) in pending.drain() {
            let _ = sender.send(Err(anyhow!("app-server process exited")));
        }
    }

    async fn discard_unhealthy_process(&mut self) {
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.start_kill();
                let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            }
        }
        self.reset_process().await;
    }

    async fn send(&self, message: &Value) -> Result<()> {
        let stdin = self
            .stdin
            .as_ref()
            .context("JSON-RPC process is not running")?;
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut writer = stdin.lock().await;
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        Ok(())
    }
}

fn resolve_executable(name: &str) -> Option<PathBuf> {
    let candidate = PathBuf::from(name);
    if candidate.components().count() > 1 {
        return candidate.is_file().then_some(candidate);
    }
    let from_path = std::env::var_os("PATH").into_iter().flat_map(|path| {
        std::env::split_paths(&path)
            .map(|directory| directory.join(name))
            .collect::<Vec<_>>()
    });
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let common = [
        Path::new("/opt/homebrew/bin").join(name),
        Path::new("/usr/local/bin").join(name),
    ]
    .into_iter()
    .chain(home.into_iter().flat_map(|home| {
        [home.join(".local/bin"), home.join(".cargo/bin")]
            .into_iter()
            .map(move |directory| directory.join(name))
    }));
    from_path.chain(common).find(|path| path.is_file())
}

impl Drop for JsonRpcClient {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

fn parse_rpc_error(error: &Value) -> JsonRpcError {
    JsonRpcError {
        code: error.get("code").and_then(Value::as_i64).unwrap_or(-32000),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned(),
        data: error.get("data").cloned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn side_effect_timeout_is_reported_as_unknown_acknowledgement() {
        let mut client = JsonRpcClient::new(vec![
            "sh".into(),
            "-c".into(),
            "while IFS= read -r line; do sleep 1; done".into(),
        ])
        .unwrap();
        client.timeout = Duration::from_millis(20);
        client.start().await.unwrap();

        let error = client
            .request_with_unknown_ack("turn/start", json!({}))
            .await
            .unwrap_err();

        assert!(super::super::acknowledgement_is_unknown(&error));
        client.close().await.unwrap();
    }
}
