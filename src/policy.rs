//! Policy and Capability — the authorization vocabulary of the browser.
//!
//! Stage 1 semantics preserved byte-for-byte (verified by self-test and
//! tests/architecture.py). Web content has no ambient OS authority.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub subject: String,
    pub resource: String,
    pub operation: String,
    pub scope: String,
    pub lifetime: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub default_decision: Decision,
}

impl Policy {
    pub(crate) fn evaluate(&self, cap: &Capability) -> Decision {
        // Bootstrap security policy: web content has no ambient OS authority.
        // Network access is the only capability admitted by this stage.
        if !cap.subject.starts_with("web-page:") {
            return Decision::Deny;
        }
        if cap.resource == "network" && cap.operation == "connect" {
            return Decision::Allow;
        }
        self.default_decision
    }
}
