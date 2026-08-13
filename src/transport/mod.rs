pub mod codex_desktop;
pub mod jsonrpc;

use thiserror::Error;

/// The request reached the provider transport, but Watchcat did not receive a
/// trustworthy acknowledgement. The provider may already have applied it.
#[derive(Debug, Error)]
#[error("provider acknowledgement is unknown: {0}")]
pub struct AcknowledgementUnknown(pub String);

pub fn acknowledgement_is_unknown(error: &anyhow::Error) -> bool {
    error.downcast_ref::<AcknowledgementUnknown>().is_some()
}
