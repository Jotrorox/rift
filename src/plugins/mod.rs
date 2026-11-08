//! The unified plugin system. A plugin is anything that receives Rift events
//! and calls the Rift host API; execution modes differ only in how and where
//! the code runs.
pub mod api;
pub mod manifest;
