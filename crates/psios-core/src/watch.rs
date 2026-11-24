use crate::*;
use psios_api::Request;
use serde_json::Value;
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
pub fn poll(
    changes: &Arc<Changes>,
    stop: &AtomicBool,
    mut req: Request,
    read_method: &str,
    ready: impl Fn(&Value) -> Result<bool>,
    mut invoke: impl FnMut(Request) -> Result<Value>,
) -> Result<Value> {
    let timeout_ms = req
        .params
        .as_object_mut()
        .and_then(|fields| fields.remove("timeout_ms"))
        .and_then(|timeout| timeout.as_u64())
        .ok_or_else(|| err("InvalidArgument", "wait requires an integer timeout_ms"))?;
    // The ordinary reader validates all remaining fields and enforces its own policy.
    require(
        (1..=10_000).contains(&timeout_ms),
        "InvalidArgument",
        "wait must be 1 to 10000 milliseconds",
    )?;
    let prior = changes.waiting.fetch_add(1, Ordering::AcqRel);
    let _slot = Slot(changes);
    require(prior < 8, "QueueFull", "waiter limit reached")?;
    let until = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        // Sample before the read. A commit between the read and the wait cannot be lost.
        let seq = changes.seq();
        let page = invoke(Request {
            schema_version: req.schema_version.clone(),
            request_id: req.request_id.clone(),
            token: req.token.clone(),
            method: read_method.into(),
            params: req.params.clone(),
        })?;
        if ready(&page)? || page["lifecycle"] == "Closed" || Instant::now() >= until {
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
