//! Provider-neutral recovery engine for interrupted coding-agent sessions.

pub mod client;
pub mod conditions;
pub mod config;
pub mod daemon;
pub mod engine;
pub mod models;
pub mod protocol;
pub mod providers;
pub mod state;
pub mod transport;

pub use models::{
    BackoffKind, Failure, InterruptReceipt, MessageDelivery, MessageReceipt, MessageTransport,
    PolicyAction, ResumeReceipt, Session, SessionLog, SessionState, TurnOutcome, WatchTarget,
};
