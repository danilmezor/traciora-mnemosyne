//! Rust-owned exact-runtime controller and optional embedded reference backend.
pub use mnemosyne_runtime_protocol::*;
#[cfg(unix)]
mod node_child;
#[cfg(unix)]
mod root_lease;
#[cfg(unix)]
pub use node_child::{
    NodeChild, NodeChildConfig, NodeHostDispatcher, NodeIdentity, RendererConfig,
    RuntimeConsoleEvent, StopReceipt,
};
#[cfg(unix)]
pub use root_lease::{RootLease, RootedHostError};
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("JavaScript runtime initialization failed: {0}")]
    Initialization(String),
    #[error("JavaScript event loop failed: {0}")]
    EventLoop(String),
    #[error("JavaScript worker failed: {0}")]
    Worker(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalOutcome {
    pub body: String,
}
impl EvalOutcome {
    /// Native pipe framing writes nothing for an empty body and avoids adding
    /// another newline when the body already ends in one.
    pub fn pipe_output(&self) -> String {
        if self.body.is_empty() || self.body.ends_with('\n') {
            self.body.clone()
        } else {
            format!("{}\n", self.body)
        }
    }
}

#[cfg(feature = "embedded-v8")]
mod embedded;
#[cfg(feature = "embedded-v8")]
pub use embedded::{
    HostCallbacks, Runtime, RuntimeHandle, TARGET_V8_VERSION, backend_version, initialize_platform,
};
