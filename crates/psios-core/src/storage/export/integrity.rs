use super::{Index, same_source};
use crate::{Error, Result, err, require};
use psios_api::ResourceRef;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    fs::{File, Metadata},
    mem,
    os::unix::fs::FileExt,
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};

const CACHE_SLOTS: usize = 4;
const VERIFY_WINDOW_BYTES: u64 = 16 * 1024 * 1024;
const VERIFY_WINDOW_TIME: Duration = Duration::from_millis(250);

type Slot = Arc<Mutex<Option<Index>>>;

#[derive(Default)]
pub(crate) struct Cache(Mutex<VecDeque<(ResourceRef, Slot)>>);

impl Cache {
    pub(super) fn checkout(&self, reference: &ResourceRef) -> Result<Slot> {
        let mut slots = self
            .0
            .lock()
            .map_err(|_| err("RecoveryRequired", "backup export cache was interrupted"))?;
        if let Some(position) = slots.iter().position(|(key, _)| key == reference) {
            let entry = slots.remove(position).unwrap();
            let slot = Arc::clone(&entry.1);
            slots.push_back(entry);
            return Ok(slot);
        }
        if slots.len() == CACHE_SLOTS {
            // The pool owns the only reference to an idle slot.
            let oldest_idle = slots
                .iter()
                .position(|(_, slot)| Arc::strong_count(slot) == 1)
                .ok_or_else(|| backoff("all backup export readers are busy"))?;
            drop(slots.remove(oldest_idle));
        }
        let slot = Slot::default();
        slots.push_back((reference.clone(), Arc::clone(&slot)));
        Ok(slot)
    }
}

pub(super) fn lock_error<T>(error: TryLockError<T>) -> Error {
    match error {
        TryLockError::WouldBlock => backoff("backup export reader is busy"),
        TryLockError::Poisoned(_) => {
            err("RecoveryRequired", "backup export reader was interrupted")
        }
    }
}

fn backoff(message: &str) -> Error {
    let mut error = err("QuotaExceeded", message);
    error.0.retryable = true;
    error.0.retry_condition = "SameRequestAfterBackoff".into();
    error
}

#[derive(Default)]
pub(super) enum MemberIntegrity {
    #[default]
    Unverified,
    Checking(Box<Verification>),
    Verified(Metadata),
}

pub(super) struct Verification {
    source: Metadata,
    offset: u64,
    hash: Sha256,
}

impl MemberIntegrity {
    pub(super) fn verify(
        &mut self,
        file: &File,
        source: &Metadata,
        total: u64,
        expected: &str,
    ) -> Result<()> {
        if matches!(self, Self::Verified(previous) if same_source(previous, source)) {
            return Ok(());
        }
        let mut verification = match mem::take(self) {
            Self::Checking(previous) if same_source(&previous.source, source) => *previous,
            _ => Verification {
                source: source.clone(),
                offset: 0,
                hash: Sha256::new(),
            },
        };
        let start = Instant::now();
        let end = total.min(verification.offset.saturating_add(VERIFY_WINDOW_BYTES));
        let mut buffer = [0u8; 128 * 1024];
        while verification.offset < end {
            let len = (end - verification.offset).min(buffer.len() as u64) as usize;
            file.read_exact_at(&mut buffer[..len], verification.offset)?;
            verification.hash.update(&buffer[..len]);
            verification.offset += len as u64;
            if start.elapsed() >= VERIFY_WINDOW_TIME {
                break;
            }
        }
        require(
            same_source(source, &file.metadata()?),
            "PreconditionFailed",
            "backup member changed during integrity verification",
        )?;
        if verification.offset < total {
            let mut error = backoff("backup member integrity verification is in progress");
            error.0.details = Some(json!({
                "backup_verified_bytes": verification.offset,
                "backup_member_bytes": total
            }));
            *self = Self::Checking(Box::new(verification));
            return Err(error);
        }
        let digest = format!("sha256:{:x}", verification.hash.finalize());
        require(
            digest == expected,
            "PreconditionFailed",
            "backup member integrity failed",
        )?;
        *self = Self::Verified(verification.source);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_cache_never_evicts_an_active_backup() {
        let cache = Cache::default();
        let keys: Vec<_> = (0..=CACHE_SLOTS)
            .map(|index| ResourceRef {
                resource_id: format!("backup-{index}"),
                version: "r1".into(),
                digest: format!("sha256:{index:064x}"),
            })
            .collect();
        let mut held: Vec<_> = keys[..CACHE_SLOTS]
            .iter()
            .map(|key| cache.checkout(key).unwrap())
            .collect();
        let error = cache.checkout(&keys[CACHE_SLOTS]).err().unwrap();
        assert_eq!(error.0.code, "QuotaExceeded");
        assert!(error.0.retryable);
        assert!(Arc::ptr_eq(&held[0], &cache.checkout(&keys[0]).unwrap()));

        drop(held.pop());
        cache.checkout(&keys[CACHE_SLOTS]).unwrap();
        assert!(Arc::ptr_eq(&held[0], &cache.checkout(&keys[0]).unwrap()));
        assert!(
            cache
                .0
                .lock()
                .unwrap()
                .iter()
                .all(|(key, _)| key != &keys[CACHE_SLOTS - 1])
        );
    }
}
