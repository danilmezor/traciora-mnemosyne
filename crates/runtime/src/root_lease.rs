//! Owned leases contain Rust IDs only. N-API roots remain on the JS thread.
use crate::{HostError, HostValue, JsReference, JsValueId};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Debug)]
pub(crate) struct LeaseOwner {
    pub(crate) alive: AtomicBool,
}
#[derive(Debug)]
struct Lease {
    reference: JsReference,
    owner: Arc<LeaseOwner>,
    released: mpsc::Sender<JsValueId>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        // Never block a destructor on JS or borrow its owning runtime. A close
        // has already destroyed the environment; later drops are harmless.
        if self.owner.alive.load(Ordering::Acquire) {
            let _ = self.released.send(self.reference.id);
        }
    }
}
#[derive(Clone, Debug)]
pub struct RootLease(Arc<Lease>);
impl RootLease {
    pub(crate) fn new(
        reference: JsReference,
        owner: Arc<LeaseOwner>,
        released: mpsc::Sender<JsValueId>,
    ) -> Self {
        Self(Arc::new(Lease {
            reference,
            owner,
            released,
        }))
    }
    pub fn reference(&self) -> JsReference {
        self.0.reference
    }
    pub fn value(&self) -> HostValue {
        HostValue::Reference(self.reference())
    }
    pub fn is_alive(&self) -> bool {
        self.0.owner.alive.load(Ordering::Acquire)
    }
    pub(crate) fn belongs_to(&self, owner: &Arc<LeaseOwner>) -> bool {
        Arc::ptr_eq(&self.0.owner, owner) && self.is_alive()
    }
}

/// The error keeps original thrown references alive without invoking their
/// getters or coercing them to error strings. Ordinary borrowed-root ownership
/// is released separately, including ownership required by in-flight replies.
#[derive(Clone, Debug)]
pub struct RootedHostError {
    pub error: HostError,
    pub roots: Vec<RootLease>,
}
impl std::fmt::Display for RootedHostError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.error.name.is_empty() && self.error.message.is_empty() {
            formatter.write_str("Original JavaScript exception")
        } else if self.error.name.is_empty() {
            formatter.write_str(&self.error.message)
        } else {
            write!(formatter, "{}: {}", self.error.name, self.error.message)
        }
    }
}
impl std::error::Error for RootedHostError {}
