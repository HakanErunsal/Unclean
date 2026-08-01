#![doc = "Provides the shared product rules used by the Unclean console and desktop frontends."]

pub mod apply;
pub mod backups;
pub mod dependencies;
pub mod descriptors;
pub mod discovery;
pub mod elevation;
pub mod error;
pub mod journal;
pub mod plans;
mod platform;
pub mod preset_catalog;
pub mod presets;
pub mod project_plans;
pub mod project_presets;
pub mod project_state;
pub mod projects;
pub mod templates;

pub use error::{Error, ErrorCode, Result};

/// Names the product in frontend titles and machine output.
pub const PRODUCT_NAME: &str = "Unclean";

/// Returns the package version embedded by Cargo.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
