//! A private, single-worker outbox for a completed local provider decision.
use super::{DecisionRequest, SessionModel, Turn};
use crate::{ApiError, ApiResult, Client, parse};
use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags, open, openat, renameat_with, unlinkat};
use rustix::io::Errno;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Write},
    os::unix::fs::MetadataExt,
    path::Path,
};

const PENDING: &str = "psios-model-pending.json";
const STAGING: &str = ".psios-model-pending.tmp";
const LIMIT: u64 = 72 * 1024;

fn storage_error(_: impl std::fmt::Display) -> Box<ApiError> {
    ApiError::new("StorageUnavailable", "provider decision journal I/O failed").into()
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pending {
    format: u32,
    authority: String,
    provider: String,
    claim_key: String,
    request: DecisionRequest,
}

struct Journal(File);
impl Drop for Journal {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
impl Journal {
    fn open(path: &Path) -> ApiResult<Self> {
        let dir = File::from(
            open(
                path,
                OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(storage_error)?,
        );
        let meta = dir.metadata().map_err(storage_error)?;
        if meta.uid() != rustix::process::geteuid().as_raw() || meta.mode() & 0o077 != 0 {
            return Err(ApiError::new(
                "PermissionDenied",
                "expected a private, owned provider directory",
            )
            .into());
        }
        dir.try_lock().map_err(|e| match e {
            std::fs::TryLockError::WouldBlock => {
                ApiError::new("Conflict", "provider journal already in use").into()
            }
            std::fs::TryLockError::Error(e) => storage_error(e),
        })?;
        // A staging record that never reached the atomic rename was never sent.
        match unlinkat(&dir, STAGING, AtFlags::empty()) {
            Ok(()) | Err(Errno::NOENT) => {}
            Err(e) => return Err(storage_error(e)),
        }
        Ok(Self(dir))
    }

    fn pending(&self) -> ApiResult<Option<Pending>> {
        let fd = match openat(
            &self.0,
            PENDING,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(storage_error(e)),
        };
        let f = File::from(fd);
        let m = f.metadata().map_err(storage_error)?;
        if !m.is_file()
            || m.nlink() != 1
            || m.mode() & 0o077 != 0
            || m.uid() != rustix::process::geteuid().as_raw()
            || m.len() > LIMIT
        {
            return Err(ApiError::new(
                "RecoveryRequired",
                "invalid retained provider decision file",
            )
            .into());
        }
        let mut bytes = Vec::new();
        f.take(LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(storage_error)?;
        let invalid = || ApiError::new("RecoveryRequired", "invalid retained provider decision");
        if bytes.len() as u64 > LIMIT {
            return Err(invalid().into());
        }
        let record: Pending =
            serde_json::from_value(parse(&bytes).map_err(|_| invalid())?).map_err(|_| invalid())?;
        if record.format != 1 {
            return Err(invalid().into());
        }
        // A previous fsync of the renamed directory entry may have returned an
        // error. Establish its durability before allowing any retransmission.
        self.0.sync_all().map_err(storage_error)?;
        Ok(Some(record))
    }

    fn retain(&self, record: &Pending) -> ApiResult<()> {
        let mut bytes = vec![0; LIMIT as usize];
        let mut buffer = std::io::Cursor::new(bytes.as_mut_slice());
        serde_json::to_writer(&mut buffer, record).map_err(|_| {
            ApiError::new("QuotaExceeded", "decision exceeds local retention limit")
        })?;
        let size = buffer.position() as usize;
        let mut f = File::from(
            openat(
                &self.0,
                STAGING,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(storage_error)?,
        );
        f.write_all(&bytes[..size])
            .and_then(|_| f.sync_all())
            .map_err(storage_error)?;
        renameat_with(&self.0, STAGING, &self.0, PENDING, RenameFlags::NOREPLACE)
            .map_err(storage_error)?;
        self.0.sync_all().map_err(storage_error)
    }

    fn remove(&self) -> ApiResult<()> {
        unlinkat(&self.0, PENDING, AtFlags::empty()).map_err(storage_error)?;
        self.0.sync_all().map_err(storage_error)
    }
}

impl Client {
    /// Retain a model decision before transmission. Reusing the same private
    /// directory, authority, principal, session and key resumes its exact request.
    pub fn model_turn_durable(
        &self,
        session: &str,
        key: &str,
        provider: &(impl SessionModel + ?Sized),
        dir: &Path,
    ) -> ApiResult<Value> {
        let journal = Journal::open(dir)?;
        let who = self.call("Principal.WhoAmI", json!({}))?;
        let authority = who["authority_id"]
            .as_str()
            .ok_or_else(|| crate::invalid("missing authority identity"))?;
        let actor = who["principal_id"]
            .as_str()
            .ok_or_else(|| crate::invalid("missing provider identity"))?;
        let record = if let Some(pending) = journal.pending()? {
            if pending.authority != authority
                || pending.provider != actor
                || pending.request.session_id != session
                || pending.claim_key != key
            {
                return Err(ApiError::new(
                    "Conflict",
                    "journal contains a decision for another authority, provider, session or key",
                )
                .into());
            }
            pending
        } else {
            match self.decide_turn(session, key, provider)? {
                Turn::Completed(receipt) => return Ok(receipt),
                Turn::Decided(request) => {
                    let record = Pending {
                        format: 1,
                        authority: authority.to_owned(),
                        provider: actor.to_owned(),
                        claim_key: key.to_owned(),
                        request,
                    };
                    journal.retain(&record)?;
                    record
                }
            }
        };
        let answer = self.send_decision(&record.request);
        // The authority checks previously settled receipts before checking the
        // lease fence. Fenced therefore also resolves this exact pending request.
        if answer.is_ok() || answer.as_ref().is_err_and(|e| e.code == "Fenced") {
            journal.remove()?;
        }
        answer
    }
}
