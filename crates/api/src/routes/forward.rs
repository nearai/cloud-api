//! Forwarding of selected routes to cloud-platform's edge door.
//!
//! Nothing here is mounted on the router yet.

pub mod config;
pub mod table;

pub use config::ForwarderConfig;
pub use table::{Entry, ForwardTable};
