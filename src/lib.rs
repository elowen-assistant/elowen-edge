//! Elowen local edge runtime.

mod config;
mod contracts;
mod discovery;
mod events;
mod execution;
mod registration;
mod runtime;
mod sandbox;

pub(crate) use discovery::{detect_device_id, detect_device_name};
pub(crate) use registration::parse_bool;
pub use runtime::run;
pub(crate) use sandbox::SandboxMode;
