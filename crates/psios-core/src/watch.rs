use crate::*;
use psios_api::Request;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub(crate) struct Changes {
    generation: Mutex<u64>,
    ready: Condvar,
    waiting: AtomicUsize,
}
impl Changes {
    pub fn notify(&self) {
        let mut seq = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        *seq = seq.wrapping_add(1);
        self.ready.notify_all();
    }
    fn seq(&self) -> u64 {
        *self.generation.lock().unwrap_or_else(|p| p.into_inner())
    }
    fn wait_for_change(&self, previous: u64, left: Duration) {
        let lock = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        drop(
            self.ready
                .wait_timeout_while(lock, left, |n| *n == previous)
                .unwrap_or_else(|p| p.into_inner()),
        );
    }
}
struct Slot<'a>(&'a Changes);
impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Wait {
    cursor: String,
    limit: Option<u32>,
    timeout_ms: u64,
}
pub fn poll(
    changes: &Arc<Changes>,
    stop: &AtomicBool,
    req: Request,
    mut invoke: impl FnMut(Request) -> Result<Value>,
) -> Result<Value> {
    let p: Wait = decode(&req.params)?;
    require(
        (1..=10_000).contains(&p.timeout_ms),
        "InvalidArgument",
        "wait must be 1 to 10000 milliseconds",
    )?;
    let prior = changes.waiting.fetch_add(1, Ordering::AcqRel);
    let _slot = Slot(changes);
    require(prior < 8, "QueueFull", "event waiter limit reached")?;
    let until = Instant::now() + Duration::from_millis(p.timeout_ms);
    loop {
        // Sample before the read. A commit between the read and the wait cannot be lost.
        let seq = changes.seq();
        let page = invoke(Request {
            schema_version: req.schema_version.clone(),
            request_id: req.request_id.clone(),
            token: req.token.clone(),
            method: "Event.Poll".into(),
            params: json!({"cursor":p.cursor,"limit":p.limit}),
        })?;
        if !page["events"].as_array().unwrap().is_empty() || Instant::now() >= until {
            return Ok(page);
        }
        require(
            !stop.load(Ordering::Acquire),
            "DependencyUnavailable",
            "authority is shutting down",
        )?;
        // Time-only expiry of a grant has no database commit to signal.
        changes.wait_for_change(
            seq,
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500)),
        );
    }
}
