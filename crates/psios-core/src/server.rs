use crate::{Config, Result, Store, broker, err, schedule, workflow};
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
    time::Duration,
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
                |request| {
                    let mut authority = accepting(s, stop)?;
                    // A write-half shutdown still permits a response.
                    ensure_peer_connected(stream)?;
                    authority.request(peer, request)
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
fn worker(s: Shared, stop: Arc<AtomicBool>, tick: Duration) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let correlation = id("scheduler");
        let task = {
            let mut store = state(&s)?;
            if stop.load(Ordering::Acquire) {
                break;
            }
            store.transaction(&correlation, broker::claim)?
        };
        let Some(task) = task else {
            thread::park_timeout(tick);
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
        let receipt = work.perform(|checkpoint| {
            state(&s)?.transaction(&correlation, |db| work.checkpoint(db, checkpoint))
        });
        let done = state(&s)?.transaction(&correlation, |db| broker::finish(db, &work, receipt));
        if let Err(e) = done {
            eprintln!("psiosd {} receipt commit failed: {}", work.op.id, e.0.code);
            state(&s)?.transaction(&correlation, |db| broker::settlement_failed(db, &work))?;
        }
    }
    Ok(())
}

fn timer(s: Shared, stop: Arc<AtomicBool>) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        {
            let mut store = state(&s)?;
            if stop.load(Ordering::Acquire) {
                break;
            }
            schedule::tick(&mut store)?;
            workflow::tick(&mut store)?;
        }
        thread::park_timeout(Duration::from_millis(250));
    }
    Ok(())
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
    let (listener, _endpoint) = bind(&cfg.socket)?;
    crate::storage::reclaim_on_startup(&cfg);
    db.transaction("restart-recovery", broker::recover)?;
    let db = Arc::new(Mutex::new(db));
    let mut workers = Vec::new();
    // Withhold the authority from workers until every required thread exists.
    let startup_guard = state(&db)?;
    let startup = (|| -> std::io::Result<()> {
        let scheduling = db.clone();
        let shutdown = stop.clone();
        workers.push(thread::Builder::new().spawn(move || timer(scheduling, shutdown))?);
        let tick = Duration::from_millis(cfg.tick_ms);
        for _ in 0..cfg.workers {
            let running = db.clone();
            let shutdown = stop.clone();
            workers.push(thread::Builder::new().spawn(move || worker(running, shutdown, tick))?);
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
    state(&db)?.changes.notify();
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
