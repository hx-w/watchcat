//! Provider-neutral recovery engine for interrupted coding-agent sessions.

pub mod config;
pub mod engine;
pub mod models;
pub mod providers;
pub mod state;
pub mod transport;

pub use models::{Failure, ResumeReceipt, Session, SessionState, WatchTarget};
