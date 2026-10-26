//! Read models of the client: what a scope binds and whether its cache answers.

use serde::Serialize;

/// One file the binding owns in a scope, and whether it currently holds the managed content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Binding {
    /// `environment`, `cargo` or `gradle`.
    pub kind: &'static str,
    pub bound: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The loopback port of the base, either the cache listener or a forward.
    pub port: u16,
    /// Whether `/v1/ready` answered `200` on that port.
    pub ready: bool,
    pub bindings: Vec<Binding>,
}
