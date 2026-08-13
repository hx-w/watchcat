use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::protocol::{MAX_FRAME_BYTES, PROTOCOL_VERSION, RpcRequest, RpcResponse};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(35);

#[derive(Clone, Debug)]
pub struct WatchcatClient {
    socket_path: PathBuf,
}

impl WatchcatClient {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[cfg(unix)]
    pub async fn request(
        &self,
        method: impl Into<String>,
        params: Value,
        expected_revision: Option<u64>,
    ) -> Result<(Value, u64)> {
        let mut stream = tokio::time::timeout(
            REQUEST_TIMEOUT,
            tokio::net::UnixStream::connect(&self.socket_path),
        )
        .await
        .context("timed out connecting to Watchcat service")?
        .with_context(|| {
            format!(
                "cannot connect to Watchcat service at {}",
                self.socket_path.display()
            )
        })?;
        let request = RpcRequest {
            version: PROTOCOL_VERSION,
            id: Uuid::new_v4().to_string(),
            method: method.into(),
            params,
            expected_revision,
        };
        let response: RpcResponse = tokio::time::timeout(REQUEST_TIMEOUT, async {
            write_frame(&mut stream, &serde_json::to_vec(&request)?).await?;
            let response = serde_json::from_slice(&read_frame(&mut stream).await?)?;
            Result::<RpcResponse>::Ok(response)
        })
        .await
        .context("Watchcat service did not respond within 35 seconds")??;
        validate_response_version(response.version)?;
        if response.id != request.id {
            bail!("Watchcat service returned a response for another request");
        }
        if let Some(error) = response.error {
            bail!("{}: {}", error.code, error.message);
        }
        Ok((response.result.unwrap_or(Value::Null), response.revision))
    }

    #[cfg(not(unix))]
    pub async fn request(
        &self,
        _method: impl Into<String>,
        _params: Value,
        _expected_revision: Option<u64>,
    ) -> Result<(Value, u64)> {
        bail!("Watchcat service transport is not available on this platform yet")
    }
}

fn validate_response_version(version: u32) -> Result<()> {
    if version != PROTOCOL_VERSION {
        bail!("Watchcat service protocol {version} is not supported; expected {PROTOCOL_VERSION}");
    }
    Ok(())
}

pub async fn write_frame<W>(writer: &mut W, bytes: &[u8]) -> Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    if bytes.len() > MAX_FRAME_BYTES {
        bail!("RPC frame exceeds {} bytes", MAX_FRAME_BYTES);
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>>
where
    R: AsyncReadExt + Unpin,
{
    read_frame_with_limit(reader, MAX_FRAME_BYTES).await
}

pub async fn read_frame_with_limit<R>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>>
where
    R: AsyncReadExt + Unpin,
{
    let length = reader.read_u32().await? as usize;
    if length > max_bytes {
        bail!("RPC frame exceeds {max_bytes} bytes");
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frames_round_trip() {
        let (mut left, mut right) = tokio::io::duplex(256);
        let writer = tokio::spawn(async move { write_frame(&mut left, b"watchcat").await });
        assert_eq!(read_frame(&mut right).await.unwrap(), b"watchcat");
        writer.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn request_frame_limit_is_checked_before_allocation() {
        let (mut left, mut right) = tokio::io::duplex(16);
        let writer = tokio::spawn(async move {
            left.write_u32(1_025).await.unwrap();
        });
        let error = read_frame_with_limit(&mut right, 1_024).await.unwrap_err();
        assert!(error.to_string().contains("exceeds 1024 bytes"));
        writer.await.unwrap();
    }

    #[test]
    fn rejects_an_incompatible_response_protocol() {
        let error = validate_response_version(PROTOCOL_VERSION + 1).unwrap_err();
        assert!(error.to_string().contains("is not supported"));
    }
}
