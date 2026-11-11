//! The compatibility command reports the pinned application/build contract.
//! Cargo package versions track this independent Rust implementation separately.
pub const APP_VERSION: &str = "1.14.4";
pub const BUILD_VERSION: &str = "1.14.4";
pub fn response() -> String {
    format!("{APP_VERSION} (installer {BUILD_VERSION})")
}
