//! Read models of the client: what a scope binds and whether its cache answers.

use serde::Serialize;

/// One file the binding owns in a scope, and whether it holds exactly the projection of the base
/// while the same tool's files that rank next to it leave that in force.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Binding {
    /// `environment`, `cargo`, `npm`, `pip`, `go` or `gradle`.
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
