//! Broker — the only path from a capability request to a decision.
//!
//! The requester cannot supply its own authorization decision; the Broker
//! delegates evaluation to the Policy (stage 1 invariant).

use crate::policy::{Capability, Decision, Policy};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Broker {
    pub policy: Policy,
}

impl Broker {
    pub(crate) fn request(&self, cap: &Capability) -> Decision {
        self.policy.evaluate(cap)
    }
}
