//! Process-lifetime request counters, read through `GET /v1/health`.
//!
//! The keys are the matched route templates of the fixed route table and the outcome classes that
//! the request events already carry, so the set is bounded and the counters cannot drift from the
//! events. Nothing identifies a request, client, project or package (STD-0011 R-0017). The
//! counters live in memory and start from zero when the process starts; they are an observation,
//! not a receipt.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;

use crate::cache::Served;

/// The template of requests outside every declared route.
pub const UNMATCHED: &str = "unmatched";

/// The outcome class of one request, one per request event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    Miss,
    Joined,
    Revalidated,
    Stale,
    /// A document the service generates itself, such as the crates index configuration.
    Answered,
    Rejected,
    Failed,
}

impl Outcome {
    const ALL: [Self; 8] = [
        Self::Hit,
        Self::Miss,
        Self::Joined,
        Self::Revalidated,
        Self::Stale,
        Self::Answered,
        Self::Rejected,
        Self::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Joined => "joined",
            Self::Revalidated => "revalidated",
            Self::Stale => "stale",
            Self::Answered => "answered",
            Self::Rejected => "rejected",
            Self::Failed => "failed",
        }
    }
}

impl From<Served> for Outcome {
    fn from(served: Served) -> Self {
        match served {
            Served::Hit => Self::Hit,
            Served::Miss => Self::Miss,
            Served::Joined => Self::Joined,
            Served::Revalidated => Self::Revalidated,
            Served::Stale => Self::Stale,
        }
    }
}

#[derive(Default)]
struct Route {
    outcomes: [AtomicU64; Outcome::ALL.len()],
    sent: AtomicU64,
    filled: AtomicU64,
}

/// Counters per route template; every template exists from the start, so counting never locks.
pub struct Counters {
    started_at_ms: u64,
    routes: BTreeMap<&'static str, Route>,
}

impl Counters {
    /// Counters for `templates`, the unmatched template and nothing else.
    pub fn new(templates: impl IntoIterator<Item = &'static str>) -> Self {
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
            });
        let routes = templates
            .into_iter()
            .chain([UNMATCHED])
            .map(|template| (template, Route::default()))
            .collect();
        Self {
            started_at_ms,
            routes,
        }
    }

    fn route(&self, template: &'static str) -> &Route {
        // Templates come from the same fixed table the counters were built from.
        self.routes
            .get(template)
            .or_else(|| self.routes.get(UNMATCHED))
            .expect("the unmatched template always exists")
    }

    /// Counts one request of `template` with `outcome`.
    pub fn outcome(&self, template: &'static str, outcome: Outcome) {
        self.route(template).outcomes[outcome as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Counts response body bytes handed to a client connection.
    pub fn sent(&self, template: &'static str, bytes: u64) {
        self.route(template)
            .sent
            .fetch_add(bytes, Ordering::Relaxed);
    }

    /// Counts the bytes of an object committed to the store from upstream.
    pub fn filled(&self, template: &'static str, bytes: u64) {
        self.route(template)
            .filled
            .fetch_add(bytes, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Snapshot {
        let routes = self
            .routes
            .iter()
            .map(|(template, route)| {
                let outcomes = Outcome::ALL
                    .iter()
                    .map(|outcome| {
                        (
                            outcome.as_str(),
                            route.outcomes[*outcome as usize].load(Ordering::Relaxed),
                        )
                    })
                    .collect();
                let counts = RouteSnapshot {
                    outcomes,
                    bytes_sent: route.sent.load(Ordering::Relaxed),
                    bytes_filled: route.filled.load(Ordering::Relaxed),
                };
                (*template, counts)
            })
            .collect();
        Snapshot {
            started_at_ms: self.started_at_ms,
            resets_on_restart: true,
            routes,
        }
    }
}

/// The counters since `started_at_ms` (Unix epoch milliseconds).
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub started_at_ms: u64,
    pub resets_on_restart: bool,
    pub routes: BTreeMap<&'static str, RouteSnapshot>,
}

/// One route template. `bytes_sent` counts response bodies handed to client connections and
/// `bytes_filled` counts objects committed from upstream; their difference is the upstream
/// traffic the store saved.
#[derive(Debug, Serialize)]
pub struct RouteSnapshot {
    pub outcomes: BTreeMap<&'static str, u64>,
    pub bytes_sent: u64,
    pub bytes_filled: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_template_reports_every_outcome_from_zero() {
        let counters = Counters::new(["/a/{path}", "/b/{path}"]);
        counters.outcome("/a/{path}", Outcome::Hit);
        counters.outcome("/a/{path}", Outcome::Hit);
        counters.outcome("/b/{path}", Served::Joined.into());
        counters.sent("/a/{path}", 17);
        counters.filled("/a/{path}", 17);
        let snapshot = counters.snapshot();
        assert_eq!(
            snapshot.routes.keys().copied().collect::<Vec<_>>(),
            ["/a/{path}", "/b/{path}", UNMATCHED]
        );
        let a = &snapshot.routes["/a/{path}"];
        assert_eq!(a.outcomes["hit"], 2);
        assert_eq!(a.outcomes.len(), Outcome::ALL.len());
        assert_eq!((a.bytes_sent, a.bytes_filled), (17, 17));
        assert_eq!(snapshot.routes["/b/{path}"].outcomes["joined"], 1);
        assert!(snapshot.resets_on_restart);
    }

    #[test]
    fn an_undeclared_template_is_counted_as_unmatched() {
        let counters = Counters::new(["/a/{path}"]);
        counters.outcome("/other", Outcome::Rejected);
        assert_eq!(
            counters.snapshot().routes[UNMATCHED].outcomes["rejected"],
            1
        );
    }
}
