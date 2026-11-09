//! Rift permissions for [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin).
//!
//! Built as a `cdylib`, this crate is a native Pumpkin plugin that answers
//! every `PlayerPermissionCheckEvent` from a replica of the Rift proxy's
//! permission data (`rift-permissions-client`), forwards `/perms` to the
//! proxy, and offers [`RiftPermissionsService`] to other plugins. See
//! `docs/permissions-pumpkin.md` for installation, configuration and the
//! service API.
//!
//! Other plugins depend on it as a library with `default-features = false`
//! (so they do not export this plugin's entry points) and fetch the service
//! at runtime:
//!
//! ```rust,ignore
//! use rift_permissions_pumpkin::{RiftPermissions, service};
//!
//! if let Some(perms) = service(&context).await {
//!     let fly = perms.check_player(&player, "myplugin:fly").as_bool(false);
//! }
//! ```

pub mod check;
pub mod command;
pub mod config;
pub mod contexts;
pub mod forwarding;
#[cfg(feature = "plugin")]
mod logging;
pub mod presence;
pub mod registry;
mod service;

#[cfg(feature = "plugin")]
pub mod plugin;

pub use contexts::{CalculatorHandle, PlayerState};
pub use rift_permissions;
pub use rift_permissions_client::{self, Error, Listener, RiftPermissions};
pub use service::{PLUGIN_NAME, PumpkinPermissions, RiftPermissionsService, SERVICE_NAME, service};
