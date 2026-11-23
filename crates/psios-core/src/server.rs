use crate::{Config, Result, Store, broker, err};
use psios_api::{ApiError, Request, id, parse, read_frame, write_frame};
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
fn state(s: &Shared) -> Result<MutexGuard<'_, Store>> {
    s.lock()
        .map_err(|_| err("RecoveryRequired", "authority mutex poisoned"))
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
        let read = match r.method.as_str() {
            "Event.Wait" => Some(("Event.Poll", "events")),
            "Mailbox.Wait" => Some(("Mailbox.Read", "items")),
            _ => None,
        };
        if let Some((method, items)) = read {
            let changes = state(s)?.changes.clone();
            return crate::watch::poll(&changes, stop, r, method, items, |request| {
                state(s)?.request(peer, request)
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
        let receipt = work.perform(|engine, cid| {
            state(&s)?.transaction(&correlation, |db| work.checkpoint_start(db, engine, cid))
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
    db.transaction("restart-recovery", broker::recover)?;
    let db = Arc::new(Mutex::new(db));
    let mut workers = Vec::new();
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
