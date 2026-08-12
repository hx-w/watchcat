//! Provider-neutral recovery engine for interrupted coding-agent sessions.

pub mod conditions;
pub mod config;
pub mod engine;
pub mod models;
pub mod providers;
pub mod state;
pub mod transport;

pub use models::{
    BackoffKind, Failure, PolicyAction, ResumeReceipt, Session, SessionLog, SessionState,
    WatchTarget,
};
