use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::fs::{FileTypeExt, MetadataExt};
#[cfg(unix)]
use std::path::PathBuf;
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(windows)]
use tokio::net::windows::named_pipe::ClientOptions;

const INITIAL_CLIENT_ID: &str = "initializing-client";
const LOCAL_HOST_ID: &str = "local";
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DesktopMessageDelivery {
    Started,
    Steered,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesktopMessageReceipt {
    pub turn_id: String,
    pub delivery: DesktopMessageDelivery,
}

#[derive(Debug, Error)]
#[error("Codex Desktop IPC {method} failed: {message}")]
pub struct DesktopIpcRemoteError {
    pub method: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TurnStateConflict {
    Inactive,
    Active,
}

pub struct CodexDesktopIpc {
    stream: Box<dyn AsyncStream>,
    client_id: String,
    timeout: Duration,
}

impl CodexDesktopIpc {
    /// Connects to the current user's Codex Desktop IPC router.
    ///
    /// `Ok(None)` means Desktop is not exposing a local router. A present but
    /// malformed or insecure endpoint is an error and must not be ignored.
    pub async fn connect_default() -> Result<Option<Self>> {
        let Some(stream) = connect_platform().await? else {
            return Ok(None);
        };
        let mut client = Self {
            stream,
            client_id: INITIAL_CLIENT_ID.into(),
            timeout: REQUEST_TIMEOUT,
        };
        client
            .initialize()
            .await
            .context("Codex Desktop IPC is present but incompatible")?;
        Ok(Some(client))
    }

    pub async fn probe() -> Result<bool> {
        Ok(Self::connect_default().await?.is_some())
    }

    /// Sends a user-authored message through the Desktop window that owns the
    /// thread. Returns `None` when no Desktop owner is registered.
    pub async fn send_message(
        &mut self,
        session_id: &str,
        message: &str,
        cwd: &str,
    ) -> Result<Option<DesktopMessageReceipt>> {
        let Some(owner) = self.discover_owner(session_id).await? else {
            return Ok(None);
        };
        let message_id = Uuid::new_v4().to_string();

        match self
            .steer(&owner, session_id, &message_id, message, cwd)
            .await
        {
            Ok(receipt) => Ok(Some(receipt)),
            Err(error) if turn_state_conflict(&error) == Some(TurnStateConflict::Inactive) => {
                match self
                    .start_turn(&owner, session_id, &message_id, message)
                    .await
                {
                    Ok(receipt) => Ok(Some(receipt)),
                    // The state can change between steer and start. One
                    // bounded reverse attempt avoids duplicate turns.
                    Err(error)
                        if turn_state_conflict(&error) == Some(TurnStateConflict::Active) =>
                    {
                        self.steer(&owner, session_id, &message_id, message, cwd)
                            .await
                            .map(Some)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// Starts a new turn through the Desktop owner without ever steering an
    /// already-running turn. This is the safe path for unattended recovery.
    pub async fn start_recovery(
        &mut self,
        session_id: &str,
        message: &str,
    ) -> Result<Option<String>> {
        let Some(owner) = self.discover_owner(session_id).await? else {
            return Ok(None);
        };
        let message_id = Uuid::new_v4().to_string();
        self.start_turn(&owner, session_id, &message_id, message)
            .await
            .map(|receipt| Some(receipt.turn_id))
    }

    pub async fn interrupt(
        &mut self,
        session_id: &str,
        expected_turn_id: &str,
    ) -> Result<Option<String>> {
        let Some(owner) = self.discover_owner(session_id).await? else {
            return Ok(None);
        };
        let response = self
            .request(
                "thread-follower-interrupt-turn",
                4,
                json!({
                    "conversationId": session_id,
                    "mode": "user-stop",
                    "expectedTurnId": expected_turn_id,
                }),
                Some(&owner),
            )
            .await?;
        let turn_id = response
            .pointer("/result/result/interruptedTurnId")
            .or_else(|| response.pointer("/result/interruptedTurnId"))
            .or_else(|| response.pointer("/interruptedTurnId"))
            .and_then(Value::as_str)
            .context("Codex Desktop accepted interrupt without a turn id")?;
        Ok(Some(turn_id.into()))
    }

    async fn initialize(&mut self) -> Result<()> {
        let response = self
            .request("initialize", 0, json!({"clientType": "watchcat"}), None)
            .await?;
        self.client_id = response
            .pointer("/result/clientId")
            .and_then(Value::as_str)
            .context("Codex Desktop IPC initialize returned no client id")?
            .into();
        Ok(())
    }

    async fn discover_owner(&mut self, session_id: &str) -> Result<Option<String>> {
        match self
            .request(
                "thread-owner-discovery",
                1,
                json!({"hostId": LOCAL_HOST_ID, "conversationId": session_id}),
                None,
            )
            .await
        {
            Ok(response) => response
                .pointer("/result/handledByClientId")
                .or_else(|| response.get("handledByClientId"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .map(Some)
                .context("Codex Desktop owner discovery returned no owner id"),
            Err(error) if remote_error_message(&error) == Some("no-client-found") => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn steer(
        &mut self,
        owner: &str,
        session_id: &str,
        message_id: &str,
        message: &str,
        cwd: &str,
    ) -> Result<DesktopMessageReceipt> {
        let input = text_input(message);
        let response = self
            .request(
                "thread-follower-steer-turn",
                1,
                json!({
                    "conversationId": session_id,
                    "clientUserMessageId": message_id,
                    "input": input,
                    "serviceTier": null,
                    "attachments": [],
                    "additionalContext": {},
                    "restoreMessage": restore_message(message_id, message, cwd),
                }),
                Some(owner),
            )
            .await?;
        let turn_id = response
            .pointer("/result/result/turnId")
            .or_else(|| response.pointer("/result/turnId"))
            .or_else(|| response.pointer("/turnId"))
            .and_then(Value::as_str)
            .context("Codex Desktop accepted steer without a turn id")?;
        Ok(DesktopMessageReceipt {
            turn_id: turn_id.into(),
            delivery: DesktopMessageDelivery::Steered,
        })
    }

    async fn start_turn(
        &mut self,
        owner: &str,
        session_id: &str,
        message_id: &str,
        message: &str,
    ) -> Result<DesktopMessageReceipt> {
        let response = self
            .request(
                "thread-follower-start-turn",
                1,
                json!({
                    "conversationId": session_id,
                    "turnStartParams": {
                        "clientUserMessageId": message_id,
                        "input": text_input(message),
                        "attachments": [],
                        "useAppServerPermissionDefault": true,
                    },
                    "mcpAppModelContextAttachments": [],
                }),
                Some(owner),
            )
            .await?;
        let turn_id = response
            .pointer("/result/result/turn/id")
            .or_else(|| response.pointer("/result/turn/id"))
            .or_else(|| response.pointer("/turn/id"))
            .and_then(Value::as_str)
            .context("Codex Desktop accepted turn start without a turn id")?;
        Ok(DesktopMessageReceipt {
            turn_id: turn_id.into(),
            delivery: DesktopMessageDelivery::Started,
        })
    }

    async fn request(
        &mut self,
        method: &str,
        version: u64,
        params: Value,
        target_client_id: Option<&str>,
    ) -> Result<Value> {
        let request_id = Uuid::new_v4().to_string();
        let mut message = json!({
            "type": "request",
            "requestId": request_id,
            "sourceClientId": self.client_id,
            "version": version,
            "method": method,
            "params": params,
            "timeoutMs": u64::try_from(self.timeout.as_millis()).unwrap_or(u64::MAX),
        });
        if let Some(target) = target_client_id {
            message["targetClientId"] = Value::String(target.into());
        }
        write_frame(&mut self.stream, &message).await?;

        let response = tokio::time::timeout(self.timeout, self.read_response(&request_id))
            .await
            .with_context(|| format!("Codex Desktop IPC request timed out: {method}"))??;
        if response.get("resultType").and_then(Value::as_str) == Some("error") {
            return Err(DesktopIpcRemoteError {
                method: method.into(),
                message: response
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown-error")
                    .into(),
            }
            .into());
        }
        if response.get("resultType").and_then(Value::as_str) != Some("success") {
            bail!("Codex Desktop IPC returned an invalid response for {method}");
        }
        Ok(response)
    }

    async fn read_response(&mut self, request_id: &str) -> Result<Value> {
        loop {
            let message = read_frame(&mut self.stream).await?;
            match message.get("type").and_then(Value::as_str) {
                Some("response")
                    if message.get("requestId").and_then(Value::as_str) == Some(request_id) =>
                {
                    return Ok(message);
                }
                Some("client-discovery-request") => {
                    let Some(discovery_id) = message.get("requestId").and_then(Value::as_str)
                    else {
                        continue;
                    };
                    write_frame(
                        &mut self.stream,
                        &json!({
                            "type": "client-discovery-response",
                            "requestId": discovery_id,
                            "response": {"canHandle": false},
                        }),
                    )
                    .await?;
                }
                _ => {}
            }
        }
    }

    #[cfg(test)]
    async fn connect_test(stream: impl AsyncStream + 'static, timeout: Duration) -> Result<Self> {
        let mut client = Self {
            stream: Box::new(stream),
            client_id: INITIAL_CLIENT_ID.into(),
            timeout,
        };
        client.initialize().await?;
        Ok(client)
    }
}

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T> AsyncStream for T where T: AsyncRead + AsyncWrite + Unpin + Send {}

fn text_input(message: &str) -> Value {
    json!([{"type": "text", "text": message, "text_elements": []}])
}

fn restore_message(message_id: &str, message: &str, cwd: &str) -> Value {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    json!({
        "id": message_id,
        "text": message,
        "cwd": cwd,
        "createdAt": u64::try_from(created_at).unwrap_or(u64::MAX),
        "context": {
            "prompt": message,
            "addedFiles": [],
            "fileAttachments": [],
            "pastedTextAttachments": [],
            "generatedPastedTextAttachmentPaths": [],
            "ideContext": null,
            "imageAttachments": [],
            "commentAttachments": [],
            "mcpAppModelContextAttachments": [],
            "appshotContexts": [],
            "workspaceRoots": [cwd],
        },
    })
}

fn remote_error_message(error: &anyhow::Error) -> Option<&str> {
    error
        .downcast_ref::<DesktopIpcRemoteError>()
        .map(|error| error.message.as_str())
}

fn turn_state_conflict(error: &anyhow::Error) -> Option<TurnStateConflict> {
    let message = remote_error_message(error)?;
    let message = message.to_ascii_lowercase();
    if [
        "no active turn",
        "without an active turn",
        "not being streamed",
        "not currently active",
        "is not active",
        "active turn already ended",
    ]
    .iter()
    .any(|needle| message.contains(needle))
    {
        return Some(TurnStateConflict::Inactive);
    }
    if ["active turn", "already active", "already in progress"]
        .iter()
        .any(|needle| message.contains(needle))
    {
        return Some(TurnStateConflict::Active);
    }
    None
}

async fn write_frame(stream: &mut (impl AsyncWrite + Unpin), message: &Value) -> Result<()> {
    let payload = serde_json::to_vec(message)?;
    if payload.is_empty() || payload.len() > MAX_FRAME_BYTES {
        bail!(
            "Codex Desktop IPC frame has invalid size: {}",
            payload.len()
        );
    }
    let length = u32::try_from(payload.len()).context("Codex Desktop IPC frame is too large")?;
    stream.write_all(&length.to_le_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

async fn read_frame(stream: &mut (impl AsyncRead + Unpin)) -> Result<Value> {
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        bail!("Codex Desktop IPC frame has invalid size: {length}");
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).context("Codex Desktop IPC returned invalid JSON")
}

#[cfg(unix)]
async fn connect_platform() -> Result<Option<Box<dyn AsyncStream>>> {
    let endpoint = default_unix_endpoint()?;
    let Some(parent) = endpoint.parent() else {
        bail!("Codex Desktop IPC endpoint has no parent directory");
    };
    let parent_metadata = match std::fs::symlink_metadata(parent) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let socket_metadata = match std::fs::symlink_metadata(&endpoint) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let uid = unsafe { libc::geteuid() };
    if !parent_metadata.is_dir()
        || parent_metadata.uid() != uid
        || parent_metadata.mode() & 0o022 != 0
    {
        bail!(
            "refusing insecure Codex Desktop IPC directory: {}",
            parent.display()
        );
    }
    if !socket_metadata.file_type().is_socket() || socket_metadata.uid() != uid {
        bail!(
            "refusing untrusted Codex Desktop IPC endpoint: {}",
            endpoint.display()
        );
    }
    match tokio::time::timeout(REQUEST_TIMEOUT, UnixStream::connect(&endpoint)).await {
        Ok(Ok(stream)) => Ok(Some(Box::new(stream))),
        Ok(Err(error)) if is_unavailable_error(&error) => Ok(None),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => bail!("timed out connecting to Codex Desktop IPC"),
    }
}

#[cfg(unix)]
fn default_unix_endpoint() -> Result<PathBuf> {
    let codex_home = match std::env::var_os("CODEX_HOME") {
        Some(path) => PathBuf::from(path),
        None => directories::BaseDirs::new()
            .context("cannot determine the current user's home directory")?
            .home_dir()
            .join(".codex"),
    };
    if !codex_home.is_absolute() {
        bail!("CODEX_HOME must be an absolute path for Desktop IPC");
    }
    Ok(codex_home.join("ipc").join("ipc.sock"))
}

#[cfg(windows)]
async fn connect_platform() -> Result<Option<Box<dyn AsyncStream>>> {
    const ENDPOINT: &str = r"\\.\pipe\codex-ipc";
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        match ClientOptions::new().open(ENDPOINT) {
            Ok(stream) => {
                windows_identity::validate_same_user_server(&stream)?;
                return Ok(Some(Box::new(stream)));
            }
            Err(error) if is_unavailable_error(&error) => return Ok(None),
            // ERROR_PIPE_BUSY: wait briefly for an existing instance instead
            // of treating normal Desktop contention as incompatibility.
            Err(error) if error.raw_os_error() == Some(231) => {
                if tokio::time::Instant::now() >= deadline {
                    bail!("timed out connecting to busy Codex Desktop IPC pipe");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
mod windows_identity {
    use std::ffi::c_void;
    use std::mem::{size_of, size_of_val};
    use std::os::windows::io::AsRawHandle;
    use std::ptr::NonNull;

    use anyhow::{Context, Result, bail};
    use tokio::net::windows::named_pipe::NamedPipeClient;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        EqualSid, GetTokenInformation, TOKEN_QUERY, TOKEN_USER, TokenUser,
    };
    use windows_sys::Win32::System::Pipes::GetNamedPipeServerProcessId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    struct OwnedHandle(NonNull<c_void>);

    impl OwnedHandle {
        fn open_process(process_id: u32) -> Result<Self> {
            // SAFETY: OpenProcess does not retain pointers and the PID came
            // from the connected named-pipe handle.
            let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
            Self::from_nullable(handle).context("cannot inspect Codex Desktop IPC server process")
        }

        fn open_token(process: HANDLE) -> Result<Self> {
            let mut token = std::ptr::null_mut();
            // SAFETY: `token` is a valid out pointer and `process` is either a
            // live owned handle or the documented current-process pseudohandle.
            if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("cannot inspect a Desktop IPC process token");
            }
            Self::from_nullable(token).context("Desktop IPC process returned a null token")
        }

        fn from_nullable(handle: HANDLE) -> Result<Self> {
            NonNull::new(handle)
                .map(Self)
                .ok_or_else(std::io::Error::last_os_error)
                .map_err(Into::into)
        }

        fn raw(&self) -> HANDLE {
            self.0.as_ptr()
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            // SAFETY: the handle is non-null, owned by this value, and closed
            // exactly once here.
            let _ = unsafe { CloseHandle(self.raw()) };
        }
    }

    pub(super) fn validate_same_user_server(stream: &NamedPipeClient) -> Result<()> {
        let pipe = stream.as_raw_handle().cast::<c_void>();
        let mut server_process_id = 0_u32;
        // SAFETY: `pipe` remains open for the call and the PID out pointer is valid.
        if unsafe { GetNamedPipeServerProcessId(pipe, &mut server_process_id) } == 0 {
            return Err(std::io::Error::last_os_error())
                .context("cannot identify Codex Desktop IPC server");
        }
        let server_process = OwnedHandle::open_process(server_process_id)?;
        let server_token = OwnedHandle::open_token(server_process.raw())?;
        // SAFETY: GetCurrentProcess returns the documented pseudohandle, which
        // remains valid for the lifetime of this process and must not be closed.
        let current_process = unsafe { GetCurrentProcess() };
        let current_token = OwnedHandle::open_token(current_process)?;
        let server_user = token_user(&server_token)?;
        let current_user = token_user(&current_token)?;
        let server_sid = user_sid(&server_user)?;
        let current_sid = user_sid(&current_user)?;
        // SAFETY: both SIDs point into aligned token buffers that remain alive
        // until after EqualSid returns.
        if unsafe { EqualSid(server_sid, current_sid) } == 0 {
            bail!("refusing Codex Desktop IPC server owned by another Windows user");
        }
        Ok(())
    }

    fn token_user(token: &OwnedHandle) -> Result<Vec<usize>> {
        let mut required = 0_u32;
        // SAFETY: this is the documented size query with a null output buffer.
        let _ = unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &mut required,
            )
        };
        if required < u32::try_from(size_of::<TOKEN_USER>()).unwrap_or(u32::MAX) {
            bail!("Desktop IPC process token returned invalid user information size");
        }
        let word_size = size_of::<usize>();
        let words = usize::try_from(required)
            .unwrap_or(usize::MAX)
            .checked_add(word_size - 1)
            .context("Desktop IPC token information is too large")?
            / word_size;
        let mut buffer = vec![0_usize; words];
        // SAFETY: Vec<usize> provides sufficient alignment, and the buffer size
        // is at least the byte count reported by the first call.
        if unsafe {
            GetTokenInformation(
                token.raw(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                u32::try_from(size_of_val(buffer.as_slice())).unwrap_or(u32::MAX),
                &mut required,
            )
        } == 0
        {
            return Err(std::io::Error::last_os_error())
                .context("cannot read Desktop IPC process user SID");
        }
        Ok(buffer)
    }

    fn user_sid(buffer: &[usize]) -> Result<*mut c_void> {
        if size_of_val(buffer) < size_of::<TOKEN_USER>() {
            bail!("Desktop IPC process token returned truncated user information");
        }
        // SAFETY: token_user allocated an aligned Vec<usize> large enough for
        // TOKEN_USER, and GetTokenInformation initialized the structure.
        let user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
        NonNull::new(user.User.Sid)
            .map(NonNull::as_ptr)
            .context("Desktop IPC process token returned a null user SID")
    }
}

fn is_unavailable_error(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
    ) {
        return true;
    }
    #[cfg(windows)]
    {
        // ERROR_FILE_NOT_FOUND and ERROR_PATH_NOT_FOUND.
        return matches!(error.raw_os_error(), Some(2 | 3));
    }
    #[cfg(not(windows))]
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    async fn read_request(stream: &mut DuplexStream, method: &str) -> Value {
        let request = read_frame(stream).await.unwrap();
        assert_eq!(request["type"], "request");
        assert_eq!(request["method"], method);
        request
    }

    async fn respond_success(stream: &mut DuplexStream, request: &Value, result: Value) {
        write_frame(
            stream,
            &json!({
                "type": "response",
                "requestId": request["requestId"],
                "resultType": "success",
                "result": result,
            }),
        )
        .await
        .unwrap();
    }

    async fn respond_error(stream: &mut DuplexStream, request: &Value, error: &str) {
        write_frame(
            stream,
            &json!({
                "type": "response",
                "requestId": request["requestId"],
                "resultType": "error",
                "error": error,
            }),
        )
        .await
        .unwrap();
    }

    async fn initialize_router(stream: &mut DuplexStream) {
        let request = read_request(stream, "initialize").await;
        assert_eq!(request["sourceClientId"], INITIAL_CLIENT_ID);
        assert_eq!(request["version"], 0);
        assert_eq!(request["params"]["clientType"], "watchcat");
        respond_success(stream, &request, json!({"clientId": "watchcat-client"})).await;
    }

    async fn discover_owner(stream: &mut DuplexStream) {
        let request = read_request(stream, "thread-owner-discovery").await;
        assert_eq!(request["sourceClientId"], "watchcat-client");
        assert_eq!(request["version"], 1);
        assert_eq!(request["params"]["hostId"], LOCAL_HOST_ID);
        respond_success(
            stream,
            &request,
            json!({"handledByClientId": "desktop-owner"}),
        )
        .await;
    }

    #[tokio::test]
    async fn frame_reader_accepts_fragmented_payloads() {
        let (mut writer, mut reader) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            let payload = br#"{"kind":"fragmented"}"#;
            let length = u32::try_from(payload.len()).unwrap().to_le_bytes();
            writer.write_all(&length[..2]).await.unwrap();
            tokio::task::yield_now().await;
            writer.write_all(&length[2..]).await.unwrap();
            for byte in payload {
                writer.write_all(&[*byte]).await.unwrap();
            }
        });
        assert_eq!(read_frame(&mut reader).await.unwrap()["kind"], "fragmented");
        task.await.unwrap();
    }

    #[tokio::test]
    async fn frame_reader_rejects_zero_and_oversized_lengths() {
        for length in [0_u32, u32::try_from(MAX_FRAME_BYTES + 1).unwrap()] {
            let (mut writer, mut reader) = tokio::io::duplex(16);
            writer.write_all(&length.to_le_bytes()).await.unwrap();
            let error = read_frame(&mut reader).await.unwrap_err();
            assert!(error.to_string().contains("invalid size"));
        }
    }

    #[test]
    fn restore_message_has_the_minimum_desktop_context() {
        let message = restore_message("message-1", "guide", "/workspace");
        assert_eq!(message["id"], "message-1");
        assert_eq!(message["context"]["prompt"], "guide");
        assert_eq!(message["context"]["workspaceRoots"][0], "/workspace");
        assert!(message["context"]["imageAttachments"].is_array());
    }

    #[tokio::test]
    async fn message_steers_the_desktop_owner() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let request = read_request(&mut router, "thread-follower-steer-turn").await;
            assert_eq!(request["targetClientId"], "desktop-owner");
            assert_eq!(request["version"], 1);
            assert_eq!(request["params"]["conversationId"], "session-1");
            assert_eq!(request["params"]["input"][0]["text"], "continue");
            assert_eq!(
                request["params"]["restoreMessage"]["context"]["workspaceRoots"][0],
                "/workspace"
            );

            write_frame(
                &mut router,
                &json!({
                    "type": "client-discovery-request",
                    "requestId": "discovery-1",
                }),
            )
            .await
            .unwrap();
            let response = read_frame(&mut router).await.unwrap();
            assert_eq!(response["type"], "client-discovery-response");
            assert_eq!(response["response"]["canHandle"], false);

            respond_success(
                &mut router,
                &request,
                json!({
                    "method": "thread-follower-steer-turn",
                    "result": {"turnId": "turn-steered"},
                }),
            )
            .await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let receipt = client
            .send_message("session-1", "continue", "/workspace")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.turn_id, "turn-steered");
        assert_eq!(receipt.delivery, DesktopMessageDelivery::Steered);
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn idle_message_starts_a_new_turn() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let steer = read_request(&mut router, "thread-follower-steer-turn").await;
            let message_id = steer["params"]["clientUserMessageId"]
                .as_str()
                .unwrap()
                .to_owned();
            assert_eq!(steer["params"]["restoreMessage"]["id"], message_id);
            respond_error(&mut router, &steer, "no active turn to steer").await;
            let start = read_request(&mut router, "thread-follower-start-turn").await;
            assert_eq!(start["targetClientId"], "desktop-owner");
            assert_eq!(
                start["params"]["turnStartParams"]["clientUserMessageId"],
                message_id
            );
            assert_eq!(
                start["params"]["turnStartParams"]["input"][0]["text"],
                "hello"
            );
            respond_success(
                &mut router,
                &start,
                json!({
                    "method": "thread-follower-start-turn",
                    "result": {"turn": {"id": "turn-started"}},
                }),
            )
            .await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let receipt = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.turn_id, "turn-started");
        assert_eq!(receipt.delivery, DesktopMessageDelivery::Started);
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn ended_turn_race_starts_a_new_turn_with_the_same_message_id() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let steer = read_request(&mut router, "thread-follower-steer-turn").await;
            let message_id = steer["params"]["clientUserMessageId"]
                .as_str()
                .unwrap()
                .to_owned();
            respond_error(
                &mut router,
                &steer,
                "Cannot steer conversation session-1 because its active turn already ended",
            )
            .await;

            let start = read_request(&mut router, "thread-follower-start-turn").await;
            assert_eq!(
                start["params"]["turnStartParams"]["clientUserMessageId"],
                message_id
            );
            respond_success(
                &mut router,
                &start,
                json!({
                    "method": "thread-follower-start-turn",
                    "result": {"turn": {"id": "turn-after-race"}},
                }),
            )
            .await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let receipt = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.turn_id, "turn-after-race");
        assert_eq!(receipt.delivery, DesktopMessageDelivery::Started);
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn idle_to_active_race_resteers_with_the_same_message_id() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let first_steer = read_request(&mut router, "thread-follower-steer-turn").await;
            let message_id = first_steer["params"]["clientUserMessageId"]
                .as_str()
                .unwrap()
                .to_owned();
            respond_error(&mut router, &first_steer, "no active turn to steer").await;

            let start = read_request(&mut router, "thread-follower-start-turn").await;
            assert_eq!(
                start["params"]["turnStartParams"]["clientUserMessageId"],
                message_id
            );
            respond_error(&mut router, &start, "thread already has an active turn").await;

            let second_steer = read_request(&mut router, "thread-follower-steer-turn").await;
            assert_eq!(second_steer["params"]["clientUserMessageId"], message_id);
            assert_eq!(second_steer["params"]["restoreMessage"]["id"], message_id);
            respond_success(
                &mut router,
                &second_steer,
                json!({
                    "method": "thread-follower-steer-turn",
                    "result": {"turnId": "turn-after-reverse-race"},
                }),
            )
            .await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let receipt = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(receipt.turn_id, "turn-after-reverse-race");
        assert_eq!(receipt.delivery, DesktopMessageDelivery::Steered);
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn unrelated_steer_error_is_not_retried() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let steer = read_request(&mut router, "thread-follower-steer-turn").await;
            respond_error(&mut router, &steer, "permission denied").await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let error = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("permission denied"));
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn recovery_never_steers_an_active_turn() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let request = read_request(&mut router, "thread-follower-start-turn").await;
            respond_error(&mut router, &request, "thread already has an active turn").await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let error = client
            .start_recovery("session-1", "continue")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("active turn"));
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn interrupt_targets_the_expected_active_turn() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            discover_owner(&mut router).await;
            let request = read_request(&mut router, "thread-follower-interrupt-turn").await;
            assert_eq!(request["version"], 4);
            assert_eq!(request["params"]["mode"], "user-stop");
            assert_eq!(request["params"]["expectedTurnId"], "turn-active");
            respond_success(
                &mut router,
                &request,
                json!({
                    "method": "thread-follower-interrupt-turn",
                    "result": {"interruptedTurnId": "turn-active"},
                }),
            )
            .await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        assert_eq!(
            client.interrupt("session-1", "turn-active").await.unwrap(),
            Some("turn-active".into())
        );
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn missing_owner_is_a_supported_fallback() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            let request = read_request(&mut router, "thread-owner-discovery").await;
            respond_error(&mut router, &request, "no-client-found").await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        assert!(
            client
                .send_message("session-1", "hello", "/workspace")
                .await
                .unwrap()
                .is_none()
        );
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn protocol_version_mismatch_fails_closed() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            let request = read_request(&mut router, "thread-owner-discovery").await;
            respond_error(&mut router, &request, "request-version-mismatch").await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let error = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("request-version-mismatch"));
        router_task.await.unwrap();
    }

    #[tokio::test]
    async fn malformed_owner_discovery_fails_closed() {
        let (client_stream, mut router) = tokio::io::duplex(64 * 1024);
        let router_task = tokio::spawn(async move {
            initialize_router(&mut router).await;
            let request = read_request(&mut router, "thread-owner-discovery").await;
            respond_success(&mut router, &request, json!({})).await;
        });

        let mut client = CodexDesktopIpc::connect_test(client_stream, TEST_TIMEOUT)
            .await
            .unwrap();
        let error = client
            .send_message("session-1", "hello", "/workspace")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("returned no owner id"));
        router_task.await.unwrap();
    }
}
