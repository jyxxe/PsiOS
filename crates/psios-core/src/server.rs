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
type WaitCondition = fn(&Value) -> bool;
fn state(s: &Shared) -> Result<MutexGuard<'_, Store>> {
    s.lock()
        .map_err(|_| err("RecoveryRequired", "authority mutex poisoned"))
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
        "model worker disconnected before it could receive a lease",
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
        let r: Request = serde_json::from_value(parse(&bytes)?)?;
        crate::label(&r.request_id)?;
        rid = r.request_id.clone();
        let wait: Option<(&str, WaitCondition)> = match r.method.as_str() {
            "Event.Wait" => Some(("Event.Poll", |page| {
                !page["events"].as_array().unwrap().is_empty()
            })),
            "Mailbox.Wait" => Some(("Mailbox.Read", |page| {
                !page["items"].as_array().unwrap().is_empty()
            })),
            "Model.WaitNext" => Some(("Model.ClaimNext", |page| page["available"] == true)),
            _ => None,
        };
        if let Some((method, ready)) = wait {
            let changes = state(s)?.changes.clone();
            return crate::watch::poll(&changes, stop, r, method, ready, |request| {
                let mut authority = state(s)?;
                if method == "Model.ClaimNext" {
                    // A long poll can outlive its worker. Check before each
                    // budget reservation. A write-half shutdown still permits replies.
                    ensure_peer_connected(stream)?;
                }
                authority.request(peer, request)
            });
        }
        state(s)?.request(peer, r)
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
fn worker(s: Shared, stop: Arc<AtomicBool>, tick: Duration) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let correlation = id("scheduler");
        let task = state(&s)?.transaction(&correlation, broker::claim)?;
        let Some(task) = task else {
            thread::sleep(tick);
            continue;
        };
        let dispatch = state(&s)?.transaction(&correlation, |db| broker::dispatch(db, &task));
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
            state(&s)?.transaction(&correlation, |db| {
                broker::finish(
                    db,
                    &work,
                    Err(err("OutcomeUnknown", "result could not be committed")),
                )
            })?;
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
pub fn serve(cfg: Config, stop: Arc<AtomicBool>) -> Result<()> {
    let mut db = Store::open(cfg.clone())?;
    let (listener, _endpoint) = bind(&cfg.socket)?;
    crate::storage::reclaim_on_startup(&cfg);
    db.transaction("restart-recovery", broker::recover)?;
    let db = Arc::new(Mutex::new(db));
    let mut workers = Vec::new();
    let scheduling = db.clone();
    let shutdown = stop.clone();
    workers.push(thread::spawn(move || timer(scheduling, shutdown)));
    for _ in 0..cfg.workers {
        let running = db.clone();
        let shutdown = stop.clone();
        let tick = Duration::from_millis(cfg.tick_ms);
        workers.push(thread::spawn(move || worker(running, shutdown, tick)));
    }
    let mut clients: Vec<(UnixStream, thread::JoinHandle<()>)> = Vec::new();
    eprintln!("psiosd ready on {}", cfg.socket.display());
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
                    Err(e) => {
                        accept_error = Some(e);
                        break;
                    }
                };
                let s = db.clone();
                let stop = stop.clone();
                clients.push((
                    cancel,
                    thread::spawn(move || {
                        let _ = connection(&s, &mut stream, &stop);
                    }),
                ));
            }
            Ok((stream, _)) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
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
    if let Some(e) = accept_error {
        return Err(e.into());
    }
    Ok(())
}
