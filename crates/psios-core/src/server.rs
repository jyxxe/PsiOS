use crate::{
    Config, Result, Store, broker, err, node::block_device::reconcile, schedule,
    storage::retirement, watch::Changes, workflow,
};
use psios_api::{ApiError, Request, id, parse, read_frame, write_frame};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions, Permissions},
    net::Shutdown,
    os::unix::{
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

type Shared = Arc<Mutex<Store>>;

enum WaitCondition {
    Items(&'static str),
    Available,
    Task(Option<i64>),
}

fn task_revision(value: &str) -> Option<i64> {
    let revision = value.strip_prefix('r')?.parse::<i64>().ok()?;
    (revision > 0 && value == format!("r{revision}")).then_some(revision)
}

impl WaitCondition {
    fn task(params: &mut Value) -> Result<Self> {
        let version = params
            .as_object_mut()
            .and_then(|fields| fields.remove("after_version"));
        let version =
            match version {
                None | Some(Value::Null) => None,
                Some(value) => Some(value.as_str().and_then(task_revision).ok_or_else(|| {
                    err("InvalidArgument", "after_version must be a task revision")
                })?),
            };
        Ok(Self::Task(version))
    }

    fn ready(&self, value: &Value) -> Result<bool> {
        match self {
            Self::Items(key) => Ok(value[key].as_array().is_some_and(|items| !items.is_empty())),
            Self::Available => Ok(value["available"] == true),
            Self::Task(None) => Ok(psios_api::task_wait_ready(value)),
            Self::Task(Some(after)) => {
                let current = value["task_revision"]
                    .as_str()
                    .and_then(task_revision)
                    .ok_or_else(|| err("RecoveryRequired", "task revision is unavailable"))?;
                crate::require(
                    current >= *after,
                    "PreconditionFailed",
                    "task wait revision is ahead of current state",
                )?;
                Ok(current > *after)
            }
        }
    }
}

fn state(s: &Shared) -> Result<MutexGuard<'_, Store>> {
    s.lock()
        .map_err(|_| err("RecoveryRequired", "authority mutex poisoned"))
}
fn accepting<'a>(s: &'a Shared, stop: &AtomicBool) -> Result<MutexGuard<'a, Store>> {
    let store = state(s)?;
    crate::require(
        !stop.load(Ordering::Acquire),
        "DependencyUnavailable",
        "Core is shutting down",
    )?;
    Ok(store)
}
fn ensure_peer_connected(stream: &UnixStream) -> Result<()> {
    let mut fds = [PollFd::new(stream, PollFlags::empty())];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    poll(&mut fds, Some(&now)).map_err(std::io::Error::from)?;
    crate::require(
        !fds[0]
            .revents()
            .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL),
        "DependencyUnavailable",
        "client disconnected while waiting for the authority",
    )
}
fn connection(s: &Shared, stream: &mut UnixStream, stop: &AtomicBool) -> Result<()> {
    let time = Some(Duration::from_secs(3));
    stream.set_read_timeout(time)?;
    stream.set_write_timeout(time)?;
    let peer = rustix::net::sockopt::socket_peercred(&*stream)
        .map_err(std::io::Error::from)?
        .uid
        .as_raw();
    let mut rid = id("request");
    let result = (|| -> Result<Value> {
        let bytes = read_frame(stream)?;
        let mut r: Request = serde_json::from_value(parse(&bytes)?)?;
        crate::label(&r.request_id)?;
        rid = r.request_id.clone();
        if r.method == crate::storage::export::METHOD {
            let job = accepting(s, stop)?.prepare_export(peer, &r)?;
            let read = job.read();
            return accepting(s, stop)?.finish_export(peer, &r, read);
        }
        if r.method == "Operation.Reconcile" {
            let probe = accepting(s, stop)?.prepare_reconcile(peer, &r)?;
            let sample = probe.map(|job| job.read()).transpose();
            return accepting(s, stop)?.finish_reconcile(peer, &r, sample);
        }
        if r.method == "NetworkTrial.Confirm" {
            let job = accepting(s, stop)?.prepare_network_trial_confirm(peer, &r)?;
            let result = job.perform(|| {
                let mut authority = accepting(s, stop)?;
                ensure_peer_connected(stream)?;
                authority.authorize_network_trial_confirm(peer, &r, &job)
            });
            return accepting(s, stop)?.finish_network_trial_confirm(peer, &r, &job, result);
        }
        if r.method == "NetworkTrial.Rollback" {
            let job = accepting(s, stop)?.prepare_network_trial_rollback(peer, &r)?;
            let result = job.perform(|| {
                let mut authority = accepting(s, stop)?;
                ensure_peer_connected(stream)?;
                authority.authorize_network_trial_rollback(peer, &r, &job)
            });
            return accepting(s, stop)?.finish_network_trial_rollback(peer, &r, &job, result);
        }
        let wait: Option<(&str, WaitCondition)> = match r.method.as_str() {
            "Event.Wait" => Some(("Event.Poll", WaitCondition::Items("events"))),
            "Resource.Watch" if r.params.get("timeout_ms").is_some() => {
                Some(("Resource.Watch", WaitCondition::Items("events")))
            }
            "Mailbox.Wait" => Some(("Mailbox.Read", WaitCondition::Items("items"))),
            "Model.WaitNext" => Some(("Model.ClaimNext", WaitCondition::Available)),
            "Task.Wait" => Some(("Task.Get", WaitCondition::task(&mut r.params)?)),
            _ => None,
        };
        if let Some((method, ready)) = wait {
            let changes = accepting(s, stop)?.changes.clone();
            return crate::watch::poll(
                &changes,
                stop,
                r,
                method,
                |page| ready.ready(page),
                |request, principal| {
                    let mut authority = accepting(s, stop)?;
                    // A write-half shutdown still permits a response.
                    ensure_peer_connected(stream)?;
                    authority.request_wait_page(peer, request, principal)
                },
            );
        }
        accepting(s, stop)?.request(peer, r)
    })();
    let mut answer = match result {
        Ok(result) => json!({"request_id": rid, "result": result}),
        Err(mut e) => {
            e.0.request_id = rid.clone();
            json!({"request_id": rid, "error": e.0})
        }
    };
    if serde_json::to_vec(&answer)?.len() > psios_api::MAX_FRAME {
        let mut e = ApiError::new("QuotaExceeded", "response exceeds local transport limit");
        e.request_id = rid.clone();
        answer = json!({"request_id": rid, "error": e});
    }
    write_frame(stream, &serde_json::to_vec(&answer)?)?;
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}
fn dispatch_when_active(
    store: &mut Store,
    stop: &AtomicBool,
    correlation: &str,
    task: &str,
) -> Result<Option<crate::host::Work>> {
    if stop.load(Ordering::Acquire) {
        store.transaction(correlation, |db| broker::release_unstarted_claim(db, task))?;
        return Ok(None);
    }
    store.transaction(correlation, |db| broker::dispatch(db, task))
}
fn worker(s: Shared, changes: Arc<Changes>, stop: Arc<AtomicBool>, tick: Duration) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let correlation = id("scheduler");
        let (task, generation) = {
            let mut store = state(&s)?;
            if stop.load(Ordering::Acquire) {
                break;
            }
            // Sample under the same authority lock as the claim. A commit
            // after an empty claim must be visible before this worker sleeps.
            let generation = changes.seq();
            (store.transaction(&correlation, broker::claim)?, generation)
        };
        let Some(task) = task else {
            changes.wait_for_work(generation, tick, &stop);
            continue;
        };
        let dispatch = {
            let mut store = state(&s)?;
            dispatch_when_active(&mut store, &stop, &correlation, &task)
        };
        let work = match dispatch {
            Ok(Some(w)) => w,
            Ok(None) => continue,
            Err(e) => {
                state(&s)?
                    .transaction(&correlation, |db| broker::dispatch_failed(db, &task, &e))?;
                continue;
            }
        };
        // SQLite and the socket remain available while an issued host operation is in flight.
        let check_each_stage = matches!(work.action, crate::host::Action::ObserveNetworkProbe(_));
        let mut check_after = Instant::now();
        let receipt = work.perform_checked(
            |checkpoint| {
                let publication = matches!(
                    &checkpoint,
                    crate::host::Checkpoint::RestoredVolume { .. }
                        | crate::host::Checkpoint::DirectoryAllocation(_)
                        | crate::host::Checkpoint::DirectoryCreation(_)
                );
                let mut store = state(&s)?;
                let result = store.transaction(&correlation, |db| work.checkpoint(db, checkpoint));
                if publication && result.is_err() && store.transaction_is_uncertain() {
                    return Err(err(
                        "OutcomeUnknown",
                        "storage checkpoint requires witness recovery",
                    ));
                }
                result
            },
            || {
                crate::require(
                    !stop.load(Ordering::Acquire),
                    "LeaseLost",
                    "authority stopped during the host data operation",
                )?;
                if check_each_stage || Instant::now() >= check_after {
                    state(&s)?.transaction(&correlation, |db| work.check_streaming(db))?;
                    check_after = Instant::now() + Duration::from_millis(100);
                }
                Ok(())
            },
        );
        let archive = match &receipt {
            Ok(crate::host::Output::OciImage(capture)) => Some(capture.archive_id.clone()),
            _ => None,
        };
        let done =
            state(&s)?.transaction(&correlation, |db| broker::finish_host(db, &work, receipt));
        if let Err(e) = done {
            eprintln!("psiosd {} receipt commit failed: {}", work.op.id, e.0.code);
            let mut store = state(&s)?;
            // A pending witness seals the Store. In that case retain the file
            // until startup establishes which database head actually committed.
            if let Some(archive) = archive
                && let Err(error) = store.transaction(&correlation, |db| {
                    crate::artifact::files::reclaim_uncommitted(db, &archive)
                })
            {
                eprintln!(
                    "psiosd managed OCI archive settlement cleanup: {}",
                    error.0.code
                );
            }
            store.transaction(&correlation, |db| broker::settlement_failed(db, &work))?;
        }
    }
    Ok(())
}

fn timer(s: Shared, stop: Arc<AtomicBool>) -> Result<()> {
    let mut next_model_expiry = Instant::now();
    let mut next_cursor_expiry = Instant::now() + Duration::from_secs(30);
    let mut next_preparation_expiry = Instant::now();
    let mut preparation_sweep = broker::preparation::authority::Sweep::default();
    let mut next_preparation_sweep = Instant::now();
    while !stop.load(Ordering::Acquire) {
        {
            let mut store = state(&s)?;
            if stop.load(Ordering::Acquire) {
                break;
            }
            schedule::tick(&mut store)?;
            workflow::tick(&mut store)?;
            if Instant::now() >= next_model_expiry {
                if let Err(error) =
                    store.transaction("model-expiry", crate::agent::inference::watchdog)
                {
                    eprintln!("psiosd model lease expiry: {}", error.0.code);
                }
                if let Err(error) =
                    store.transaction("network-trial-expiry", broker::expire_network_trials)
                {
                    eprintln!("psiosd network trial expiry: {}", error.0.code);
                }
                next_model_expiry = Instant::now() + Duration::from_secs(1);
            }
            if Instant::now() >= next_cursor_expiry {
                if let Err(error) =
                    store.transaction("watch-scope-expiry", crate::events::scopes::reclaim)
                {
                    eprintln!("psiosd watch scope expiry: {}", error.0.code);
                }
                next_cursor_expiry = Instant::now() + Duration::from_secs(30);
            }
            if Instant::now() >= next_preparation_expiry {
                let delay = match store
                    .transaction("task-preparation-expiry", broker::preparation::collect)
                {
                    Ok(removed) if removed > 0 => Duration::from_secs(1),
                    Ok(_) => Duration::from_secs(30),
                    Err(error) => {
                        eprintln!("psiosd task preparation expiry: {}", error.0.code);
                        Duration::from_secs(30)
                    }
                };
                next_preparation_expiry = Instant::now() + delay;
            }
            if Instant::now() >= next_preparation_sweep {
                let sweep = store.transaction("task-preparation-revocation", |db| {
                    broker::preparation::authority::advance(db, &preparation_sweep)
                });
                let delay = match sweep {
                    Ok((next, more)) => {
                        preparation_sweep = next;
                        if more {
                            Duration::from_millis(250)
                        } else {
                            Duration::from_secs(1)
                        }
                    }
                    Err(error) => {
                        eprintln!("psiosd task preparation authority sweep: {}", error.0.code);
                        Duration::from_secs(1)
                    }
                };
                next_preparation_sweep = Instant::now() + delay;
            }
        }
        thread::park_timeout(Duration::from_millis(250));
    }
    Ok(())
}

fn reclaim_storage(s: Shared, stop: Arc<AtomicBool>, config: Config) -> Result<()> {
    let mut archive_cursor = String::new();
    let mut restore_cursor = String::new();
    let mut directory_cursor = String::new();
    let mut entry_cursor = crate::storage::reclamation::Cursor::default();
    let mut retirement_cursor = crate::storage::reclamation::Cursor::default();
    let mut eviction_cursor = crate::artifact::retention::delayed::Cursor::default();
    while !stop.load(Ordering::Acquire) {
        let correlation = id("storage-reclaim");
        if let Err(error) = reclaim_one_oci_archive(&s, &config, &correlation, &mut archive_cursor)
        {
            eprintln!("psiosd managed OCI archive reclamation: {}", error.0.code);
        }
        if let Err(error) =
            reclaim_one_restore_stage(&s, &config, &correlation, &mut restore_cursor)
        {
            eprintln!("psiosd restore stage reclamation: {}", error.0.code);
        }
        if !stop.load(Ordering::Acquire)
            && let Err(error) =
                reclaim_one_directory_stage(&s, &config, &correlation, &mut directory_cursor)
        {
            eprintln!(
                "psiosd directory creation stage reclamation: {}",
                error.0.code
            );
        }
        if !stop.load(Ordering::Acquire)
            && let Err(error) =
                reclaim_one_volume_entry(&s, &config, &correlation, &mut entry_cursor)
        {
            eprintln!("psiosd volume entry reclamation: {}", error.0.code);
        }
        let eviction_progress =
            match state(&s)?.transaction(&id("result-eviction-schedule"), |db| {
                if stop.load(Ordering::Acquire) {
                    return Ok(false);
                }
                crate::artifact::retention::delayed::advance(db, &mut eviction_cursor)
            }) {
                Ok(progress) => progress,
                Err(error) => {
                    eprintln!("psiosd scheduled result eviction: {}", error.0.code);
                    false
                }
            };
        if !stop.load(Ordering::Acquire)
            && let Err(error) = state(&s)?.transaction(
                "artifact-upload-authority",
                crate::artifact::ingress::authority::reclaim,
            )
        {
            eprintln!(
                "psiosd artifact upload authority reclamation: {}",
                error.0.code
            );
        }
        let ready = state(&s)?.transaction(&correlation, |db| {
            crate::artifact::ingress::expire(db)?;
            retirement::next(db, &mut retirement_cursor)
        });
        let delay = match ready {
            Ok(Some(job)) if !stop.load(Ordering::Acquire) => {
                let result = job.reclaim(&config);
                match state(&s)?.transaction(&correlation, |db| job.finish(db, result)) {
                    Ok(()) => Duration::from_millis(50),
                    Err(error) => {
                        eprintln!("psiosd storage reclamation receipt: {}", error.0.code);
                        Duration::from_secs(1)
                    }
                }
            }
            Ok(_) if eviction_progress => Duration::from_millis(50),
            Ok(_) => Duration::from_secs(1),
            Err(error) => {
                eprintln!("psiosd storage reclamation admission: {}", error.0.code);
                Duration::from_secs(1)
            }
        };
        thread::park_timeout(delay);
    }
    Ok(())
}

fn reclaim_one_volume_entry(
    s: &Shared,
    config: &Config,
    correlation: &str,
    cursor: &mut crate::storage::reclamation::Cursor,
) -> Result<()> {
    use crate::storage::entry_retirement;
    let selected = state(s)?.transaction(correlation, |db| {
        entry_retirement::next_disposal(db, cursor)
    })?;
    if let Some(disposal) = selected {
        let result = disposal.run(config);
        state(s)?.transaction(correlation, |db| disposal.finish(db, result))?;
    }
    Ok(())
}

fn reclaim_one_directory_stage(
    s: &Shared,
    config: &Config,
    correlation: &str,
    cursor: &mut String,
) -> Result<()> {
    let selected = state(s)?.transaction(correlation, |db| {
        crate::storage::directory_create::next_disposal(db, cursor)
    })?;
    if let Some(disposal) = selected {
        let result = disposal.run(config);
        state(s)?.transaction(correlation, |db| disposal.finish(db, result))?;
    }
    Ok(())
}

fn reconcile_block_devices(s: Shared, stop: Arc<AtomicBool>, interval: Duration) -> Result<()> {
    let mut cursor = reconcile::Cursor::default();
    while !stop.load(Ordering::Acquire) {
        let round = (|| {
            let (next, batch) = state(&s)?
                .transaction("block-device-reconcile", |db| reconcile::next(db, cursor))?;
            cursor = next;
            if stop.load(Ordering::Acquire) {
                return Ok(());
            }
            let observed = batch.sample()?;
            if !observed.is_empty() {
                state(&s)?.transaction("block-device-reconcile", |db| {
                    if !stop.load(Ordering::Acquire) {
                        observed.publish(db)?;
                    }
                    Ok(())
                })?;
            }
            Ok::<(), crate::Error>(())
        })();
        if let Err(error) = round {
            eprintln!("psiosd block device reconciliation: {}", error.0.code);
        }
        thread::park_timeout(interval);
    }
    Ok(())
}

fn reclaim_one_oci_archive(
    s: &Shared,
    config: &Config,
    correlation: &str,
    cursor: &mut String,
) -> Result<()> {
    let withdrawn = state(s)?.transaction(correlation, |db| {
        crate::artifact::files::next_disposal(db, cursor)
    })?;
    if let Some(id) = withdrawn {
        cursor.clone_from(&id);
        if let Some(depot) = crate::storage::oci_archive::Depot::open(&config.state_dir)? {
            depot.retire(&id)?;
        }
        state(s)?.transaction(correlation, |db| crate::artifact::files::dispose(db, &id))?;
    }
    Ok(())
}

fn reclaim_one_restore_stage(
    s: &Shared,
    config: &Config,
    correlation: &str,
    cursor: &mut String,
) -> Result<()> {
    use crate::storage::restore_publication::{Disposal, next_disposal};
    let selected = state(s)?.transaction(correlation, |db| next_disposal(db, cursor))?;
    let Some(operation) = selected else {
        cursor.clear();
        return Ok(());
    };
    cursor.clone_from(&operation);
    let job = state(s)?.transaction(correlation, |db| Disposal::open(db, &operation))?;
    let removed = job.run(config)?;
    state(s)?.transaction(correlation, |db| job.finish(db, removed))
}

struct Endpoint<'a> {
    path: &'a Path,
    _lock: File,
}
impl Drop for Endpoint<'_> {
    fn drop(&mut self) {
        let _ = fs::remove_file(self.path);
    }
}
fn bind(path: &Path) -> Result<(UnixListener, Endpoint<'_>)> {
    let dir = path
        .parent()
        .ok_or_else(|| err("InvalidArgument", "invalid socket path"))?;
    let md = fs::symlink_metadata(dir)?;
    crate::require(
        dir.canonicalize()? == dir
            && md.is_dir()
            && md.mode() & 0o022 == 0
            && md.uid() == rustix::process::geteuid().as_raw(),
        "PermissionDenied",
        "socket directory must be owned and protected",
    )?;
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    let lock = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(name)?;
    lock.try_lock()
        .map_err(|_| err("Conflict", "socket already owned by another authority"))?;
    match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_socket() && m.uid() == md.uid() => {
            crate::require(
                UnixStream::connect(path).is_err(),
                "Conflict",
                "socket already serving",
            )?;
            fs::remove_file(path)?
        }
        Ok(_) => return Err(err("Conflict", "socket path occupied by another file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let socket = UnixListener::bind(path)?;
    let ep = Endpoint { path, _lock: lock };
    fs::set_permissions(path, Permissions::from_mode(0o660))?;
    socket.set_nonblocking(true)?;
    Ok((socket, ep))
}

fn resource_pressure(error: &std::io::Error) -> bool {
    use rustix::io::Errno;
    matches!(
        error.raw_os_error().map(Errno::from_raw_os_error),
        Some(Errno::AGAIN | Errno::MFILE | Errno::NFILE | Errno::NOBUFS | Errno::NOMEM)
    )
}

pub fn serve(cfg: Config, stop: Arc<AtomicBool>) -> Result<()> {
    let mut db = Store::open(cfg.clone())?;
    db.reclaim_oci_archives()?;
    let (listener, _endpoint) = bind(&cfg.socket)?;
    let restore_stages = db.transaction(
        "restore-stage-custody",
        crate::storage::restore_publication::protected_stages,
    );
    if let Err(error) = &restore_stages {
        eprintln!("psiosd volume staging retained: {}", error.0.code);
    }
    crate::storage::reclaim_on_startup(&cfg, restore_stages.as_ref().ok());
    db.transaction("restart-recovery", |db| {
        crate::events::scopes::reclaim(db)?;
        crate::node::service_control::verify_active_holds(db)?;
        broker::recover(db)
    })?;
    let changes = db.changes.clone();
    let db = Arc::new(Mutex::new(db));
    let mut workers = Vec::new();
    // Withhold the authority from workers until every required thread exists.
    let startup_guard = state(&db)?;
    let startup = (|| -> std::io::Result<()> {
        let scheduling = db.clone();
        let shutdown = stop.clone();
        workers.push(thread::Builder::new().spawn(move || timer(scheduling, shutdown))?);
        let cleanup = db.clone();
        let shutdown = stop.clone();
        let storage = cfg.clone();
        workers.push(
            thread::Builder::new().spawn(move || reclaim_storage(cleanup, shutdown, storage))?,
        );
        if let Some(ms) = cfg.block_device_reconcile_ms {
            let running = db.clone();
            let shutdown = stop.clone();
            workers.push(thread::Builder::new().spawn(move || {
                reconcile_block_devices(running, shutdown, Duration::from_millis(ms))
            })?);
        }
        let tick = Duration::from_millis(cfg.tick_ms);
        for _ in 0..cfg.workers {
            let running = db.clone();
            let committed = changes.clone();
            let shutdown = stop.clone();
            workers.push(
                thread::Builder::new().spawn(move || worker(running, committed, shutdown, tick))?,
            );
        }
        Ok(())
    })();
    if startup.is_ok() {
        eprintln!("psiosd ready on {}", cfg.socket.display());
    } else {
        stop.store(true, Ordering::Release);
    }
    drop(startup_guard);
    let mut clients: Vec<(UnixStream, thread::JoinHandle<()>)> = Vec::new();
    let mut accept_error = None;
    while !stop.load(Ordering::Acquire) && !workers.iter().any(thread::JoinHandle::is_finished) {
        let mut i = 0;
        while i < clients.len() {
            if clients[i].1.is_finished() {
                let (_, h) = clients.swap_remove(i);
                let _ = h.join();
            } else {
                i += 1
            }
        }
        match listener.accept() {
            Ok((mut stream, _)) if clients.len() < 32 => {
                let cancel = match stream.try_clone() {
                    Ok(s) => s,
                    Err(e) if resource_pressure(&e) => {
                        let _ = stream.shutdown(Shutdown::Both);
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(e) => {
                        accept_error = Some(e);
                        break;
                    }
                };
                let s = db.clone();
                let stop = stop.clone();
                match thread::Builder::new().spawn(move || {
                    let _ = connection(&s, &mut stream, &stop);
                }) {
                    Ok(client) => clients.push((cancel, client)),
                    Err(e) if resource_pressure(&e) => {
                        let _ = cancel.shutdown(Shutdown::Both);
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => {
                        accept_error = Some(e);
                        break;
                    }
                }
            }
            Ok((stream, _)) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(e) if resource_pressure(&e) => thread::sleep(Duration::from_millis(10)),
            Err(e) => {
                accept_error = Some(e);
                break;
            }
        }
    }
    stop.store(true, Ordering::Release);
    changes.notify_shutdown();
    for worker in &workers {
        worker.thread().unpark();
    }
    drop(listener);
    for (s, h) in clients {
        let _ = s.shutdown(Shutdown::Both);
        let _ = h.join();
    }
    let mut result = Ok(());
    for worker in workers {
        if let Err(error) = worker
            .join()
            .unwrap_or_else(|_| Err(err("RecoveryRequired", "worker terminated unexpectedly")))
        {
            result = Err(error);
        }
    }
    result?;
    startup.map_err(|e| {
        err(
            "DependencyUnavailable",
            format!("could not start Core workers: {e}"),
        )
    })?;
    if let Some(e) = accept_error {
        return Err(e.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
