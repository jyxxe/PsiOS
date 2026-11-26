use crate::*;
use psios_api::Request;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const MAX_WAITERS: usize = 8;
const MAX_PER_PRINCIPAL: usize = 2;

#[derive(Default)]
pub(crate) struct Changes {
    generation: Mutex<u64>,
    ready: Condvar,
    work: Condvar,
    waiting: Mutex<HashMap<String, usize>>,
}
impl Changes {
    pub fn notify(&self) {
        let mut seq = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        *seq = seq.wrapping_add(1);
        self.ready.notify_all();
        self.work.notify_one();
    }
    pub fn notify_shutdown(&self) {
        self.notify();
        self.work.notify_all();
    }
    pub fn seq(&self) -> u64 {
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

    pub fn wait_for_work(&self, previous: u64, timeout: Duration, stop: &AtomicBool) {
        let lock = self.generation.lock().unwrap_or_else(|p| p.into_inner());
        drop(
            self.work
                .wait_timeout_while(lock, timeout, |n| {
                    *n == previous && !stop.load(Ordering::Acquire)
                })
                .unwrap_or_else(|p| p.into_inner()),
        );
    }

    fn wait_slot(&self, principal: String) -> Result<Slot<'_>> {
        let mut waiting = self.waiting.lock().unwrap_or_else(|p| p.into_inner());
        require(
            waiting.values().sum::<usize>() < MAX_WAITERS,
            "QueueFull",
            "waiter limit reached",
        )?;
        let limit = if principal == policy::OWNER {
            MAX_WAITERS
        } else {
            MAX_PER_PRINCIPAL
        };
        let count = waiting.entry(principal.clone()).or_default();
        require(
            *count < limit,
            "QueueFull",
            "principal waiter limit reached",
        )?;
        *count += 1;
        Ok(Slot {
            changes: self,
            principal,
        })
    }
}

struct Slot<'a> {
    changes: &'a Changes,
    principal: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut waiting = self
            .changes
            .waiting
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let count = waiting.get_mut(&self.principal).expect("allocated waiter");
        *count -= 1;
        if *count == 0 {
            waiting.remove(&self.principal);
        }
    }
}

pub fn poll(
    changes: &Arc<Changes>,
    stop: &AtomicBool,
    mut req: Request,
    read_method: &str,
    ready: impl Fn(&Value) -> Result<bool>,
    mut invoke: impl FnMut(Request, Option<&str>) -> Result<(String, Value)>,
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
    let until = Instant::now() + Duration::from_millis(timeout_ms);
    let mut slot: Option<Slot<'_>> = None;
    loop {
        // Sample before the read. A commit between the read and the wait cannot be lost.
        let seq = changes.seq();
        let (principal, page) = invoke(
            Request {
                schema_version: req.schema_version.clone(),
                request_id: req.request_id.clone(),
                token: req.token.clone(),
                method: read_method.into(),
                params: req.params.clone(),
            },
            slot.as_ref().map(|slot| slot.principal.as_str()),
        )?;
        if ready(&page)? || page["lifecycle"] == "Closed" || Instant::now() >= until {
            return Ok(page);
        }
        require(
            !stop.load(Ordering::Acquire),
            "DependencyUnavailable",
            "authority is shutting down",
        )?;
        // A ready response, including a committed model claim, never consumes waiter capacity.
        if slot.is_none() {
            slot = Some(changes.wait_slot(principal)?);
        }
        // Time-only expiry of a grant has no database commit to signal.
        changes.wait_for_change(
            seq,
            until
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(500)),
        );
    }
}
