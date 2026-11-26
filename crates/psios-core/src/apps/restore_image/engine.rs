use super::{Binding, Presence};
use crate::{Result, err, require};
use http_body_util::{BodyExt, Limited};
use hyper::{
    Method, Request, StatusCode,
    body::{Body, Bytes, Frame, SizeHint},
    client::conn::http1::{self, SendRequest},
    header,
};
use hyper_util::rt::TokioIo;
use rustix::fs::{MemfdFlags, SealFlags, fcntl_add_seals, memfd_create};
use serde_json::Value;
use std::{
    fs::File,
    io::{self, Read, Seek},
    path::Path,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{net::UnixStream, runtime::Builder, task::JoinHandle, time::timeout};

pub(super) const API_TIMEOUT: Duration = Duration::from_secs(5);
const LOAD_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE: usize = 128 * 1024;

pub(super) struct Input {
    file: File,
    remaining: u64,
}

impl Input {
    pub fn seal<T>(write: impl FnOnce(&mut File) -> Result<T>) -> Result<(Self, T)> {
        let (file, value) = seal_file(write)?;
        let remaining = file.metadata()?.len();
        Ok((Self { file, remaining }, value))
    }
}

pub(super) fn seal_file<T>(write: impl FnOnce(&mut File) -> Result<T>) -> Result<(File, T)> {
    let fd = memfd_create(
        "psios-oci-restore",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .map_err(io::Error::from)?;
    let mut file = File::from(fd);
    let value = write(&mut file)?;
    file.rewind()?;
    fcntl_add_seals(
        &file,
        SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL,
    )
    .map_err(io::Error::from)?;
    Ok((file, value))
}

#[derive(Default)]
struct RequestBody(Option<Input>);

impl Body for RequestBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<io::Result<Frame<Bytes>>>> {
        let Some(input) = self.0.as_mut().filter(|input| input.remaining > 0) else {
            return Poll::Ready(None);
        };
        let mut data = vec![0; input.remaining.min(32 * 1024) as usize];
        let frame = input.file.read_exact(&mut data).map(|()| {
            input.remaining -= data.len() as u64;
            Frame::data(Bytes::from(data))
        });
        Poll::Ready(Some(frame))
    }

    fn is_end_stream(&self) -> bool {
        self.0.as_ref().is_none_or(|input| input.remaining == 0)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.0.as_ref().map_or(0, |input| input.remaining))
    }
}

fn unavailable() -> crate::Error {
    err(
        "DependencyUnavailable",
        "local Docker Engine request did not complete",
    )
}

pub(in crate::apps) fn runtime() -> Result<tokio::runtime::Runtime> {
    Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(|_| unavailable())
}

pub(in crate::apps) struct Connection {
    sender: SendRequest<RequestBody>,
    driver: JoinHandle<()>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Connection {
    pub async fn open(socket: &Path) -> Result<Self> {
        timeout(API_TIMEOUT, async {
            let socket = UnixStream::connect(socket)
                .await
                .map_err(|_| unavailable())?;
            let (sender, connection) = http1::Builder::new()
                .max_buf_size(MAX_RESPONSE)
                .handshake(TokioIo::new(socket))
                .await
                .map_err(|_| unavailable())?;
            let driver = tokio::spawn(async {
                let _ = connection.await;
            });
            Ok(Self { sender, driver })
        })
        .await
        .map_err(|_| unavailable())?
    }

    // No pool or automatic reconnection. Readiness, authorization and POST all
    // refer to the same already connected Docker peer.
    async fn request(
        &mut self,
        method: Method,
        path: &str,
        input: Option<Input>,
    ) -> Result<(StatusCode, Bytes)> {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost");
        if let Some(input) = &input {
            request = request
                .header(header::CONTENT_TYPE, "application/x-tar")
                .header(header::CONTENT_LENGTH, input.remaining);
        }
        let request = request
            .body(RequestBody(input))
            .map_err(|_| unavailable())?;
        let response = self
            .sender
            .send_request(request)
            .await
            .map_err(|_| unavailable())?;
        let status = response.status();
        let data = Limited::new(response.into_body(), MAX_RESPONSE)
            .collect()
            .await
            .map_err(|_| unavailable())?
            .to_bytes();
        Ok((status, data))
    }

    async fn bound_linux_info(&mut self, expected: &str) -> Result<Value> {
        let (status, data) = timeout(API_TIMEOUT, self.request(Method::GET, "/v1.44/info", None))
            .await
            .map_err(|_| unavailable())??;
        require(
            status == StatusCode::OK,
            "DependencyUnavailable",
            "cannot read Docker Engine info",
        )?;
        let info = psios_api::parse(&data).map_err(|_| unavailable())?;
        require(
            info["ID"] == expected,
            "PreconditionFailed",
            "local Docker Engine identity differs from the recorded engine",
        )?;
        require(
            info["OSType"] == "linux",
            "UnsupportedCapability",
            "the recorded Docker Engine must manage Linux containers",
        )?;
        Ok(info)
    }

    pub(in crate::apps) async fn require_linux_engine(&mut self, expected: &str) -> Result<()> {
        self.bound_linux_info(expected).await?;
        Ok(())
    }

    pub async fn identity(&mut self, expected: &str) -> Result<()> {
        let info = self.bound_linux_info(expected).await?;
        require(
            matches!(info["Architecture"].as_str(), Some("x86_64" | "amd64"))
                && matches!(info["Driver"].as_str(), Some("overlay2" | "vfs"))
                && (info["Driver"] == "vfs" && info["DriverStatus"].is_null()
                    || info["DriverStatus"].as_array().is_some_and(|rows| {
                        rows.iter().all(|row| {
                            row.as_array().is_some_and(|fields| {
                                fields.len() == 2
                                    && fields[0].as_str().is_some_and(|key| key != "driver-type")
                                    && fields[1].is_string()
                            })
                        })
                    })),
            "UnsupportedCapability",
            "OCI restore requires the classic Linux amd64 Docker overlay2 or vfs image store",
        )
    }

    pub(super) async fn observe(&mut self, binding: &Binding) -> Result<Presence> {
        let uri = format!("/v1.44/images/{}/json", binding.image_id);
        let (status, data) = timeout(API_TIMEOUT, self.request(Method::GET, &uri, None))
            .await
            .map_err(|_| unavailable())??;
        if status == StatusCode::NOT_FOUND {
            return Ok(Presence::Absent);
        }
        require(
            status == StatusCode::OK,
            "DependencyUnavailable",
            "cannot inspect the Docker image",
        )?;
        let image = psios_api::parse(&data).map_err(|_| unavailable())?;
        require(
            image["Id"] == binding.image_id
                && image["Os"] == "linux"
                && image["Architecture"] == "amd64"
                && image["RootFS"]["Type"] == "layers"
                && image["RootFS"]["Layers"] == serde_json::json!(binding.diff_ids)
                && image.get("RepoTags").is_some_and(|tags| {
                    tags.is_null() || tags.as_array().is_some_and(Vec::is_empty)
                })
                && image["Config"].is_object()
                && image["Config"]["Volumes"]
                    .as_object()
                    .is_none_or(|volumes| volumes.is_empty()),
            "PreconditionFailed",
            "Docker inspection does not match the exact untagged release image and layer identities",
        )?;
        Ok(Presence::Present)
    }

    pub(in crate::apps) async fn matching_containers(
        &mut self,
        filter: &str,
        selector: &str,
    ) -> Result<Vec<String>> {
        let filter = serde_json::json!({(filter):[selector]}).to_string();
        let mut encoded = String::new();
        for byte in filter.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
                encoded.push(char::from(byte));
            } else {
                use std::fmt::Write as _;
                write!(encoded, "%{byte:02X}").expect("writing to String cannot fail");
            }
        }
        let (status, data) = timeout(
            API_TIMEOUT,
            self.request(
                Method::GET,
                &format!("/v1.44/containers/json?all=1&filters={encoded}"),
                None,
            ),
        )
        .await
        .map_err(|_| unavailable())??;
        require(
            status == StatusCode::OK,
            "DependencyUnavailable",
            "cannot list exact Docker candidate",
        )?;
        let rows: Vec<Value> = serde_json::from_slice(&data).map_err(|_| unavailable())?;
        rows.iter()
            .map(|row| {
                row["Id"]
                    .as_str()
                    .filter(|id| super::super::create_recovery::cid_ok(id))
                    .map(str::to_owned)
                    .ok_or_else(unavailable)
            })
            .collect()
    }

    pub(in crate::apps) async fn inspect_container(&mut self, cid: &str) -> Result<Value> {
        require(
            super::super::create_recovery::cid_ok(cid),
            "InvalidArgument",
            "OCI container ID is not exact",
        )?;
        let (status, data) = timeout(
            API_TIMEOUT,
            self.request(Method::GET, &format!("/v1.44/containers/{cid}/json"), None),
        )
        .await
        .map_err(|_| unavailable())??;
        require(
            status == StatusCode::OK,
            "DependencyUnavailable",
            "cannot inspect exact Docker candidate",
        )?;
        psios_api::parse(&data).map_err(Into::into)
    }

    pub(in crate::apps) async fn remove_unforced_container(&mut self, cid: &str) -> Result<()> {
        require(
            super::super::create_recovery::cid_ok(cid),
            "InvalidArgument",
            "OCI container ID is not exact",
        )?;
        let response = timeout(
            API_TIMEOUT,
            self.request(
                Method::DELETE,
                &format!("/v1.44/containers/{cid}?force=false&v=false"),
                None,
            ),
        )
        .await;
        require(
            matches!(response, Ok(Ok((StatusCode::NO_CONTENT, _)))),
            "OutcomeUnknown",
            "Docker did not acknowledge exact unforced removal",
        )
    }

    pub async fn ready(&mut self) -> Result<()> {
        timeout(API_TIMEOUT, self.sender.ready())
            .await
            .map_err(|_| unavailable())?
            .map_err(|_| unavailable())
    }

    pub(super) async fn load(&mut self, input: Input, image_id: &str) -> Result<()> {
        let (status, data) = timeout(
            LOAD_TIMEOUT,
            self.request(Method::POST, "/v1.44/images/load?quiet=1", Some(input)),
        )
        .await
        .map_err(|_| unavailable())??;
        require(
            status == StatusCode::OK && !data.is_empty(),
            "DependencyUnavailable",
            "Docker did not confirm image import",
        )?;
        let expected = format!("Loaded image ID: {image_id}");
        let mut identified = false;
        for line in data
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let message: Value = psios_api::parse(line).map_err(|_| unavailable())?;
            require(
                message.is_object()
                    && message.get("error").is_none()
                    && message.get("errorDetail").is_none(),
                "DependencyUnavailable",
                "Docker reported an image import error",
            )?;
            identified |= message["stream"]
                .as_str()
                .is_some_and(|stream| stream.lines().any(|line| line == expected));
        }
        require(
            identified,
            "DependencyUnavailable",
            "Docker did not report the approved image ID",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;
    use hyper::{Response, server::conn::http1 as server_http1, service::service_fn};
    use std::{
        convert::Infallible,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn shared_binding() -> Binding {
        use psios_api::ResourceRef;
        let rf = ResourceRef {
            resource_id: "resource".into(),
            version: "r1".into(),
            digest: "sha256:fixture".into(),
        };
        Binding {
            application_ref: rf.clone(),
            release_ref: rf.clone(),
            declaration_ref: rf.clone(),
            publisher_key_ref: rf.clone(),
            image_archive_ref: rf.clone(),
            image_capture_ref: rf.clone(),
            node_ref: rf,
            engine_id: "test-engine-000001".to_owned(),
            image_id: "sha256:".to_owned() + &"1".repeat(64),
            diff_ids: vec!["sha256:".to_owned() + &"2".repeat(64)],
        }
    }

    #[test]
    fn same_unix_connection_checks_engine_streams_exact_fd_and_verifies_image() {
        let temp = tempfile::tempdir().unwrap();
        let socket = temp.path().join("docker.sock");
        let (sealed, ()) = Input::seal(|file| {
            std::io::Write::write_all(file, b"exact fd contents")?;
            Ok(())
        })
        .unwrap();
        let seals = rustix::fs::fcntl_get_seals(&sealed.file).unwrap();
        assert!(
            seals
                .contains(SealFlags::WRITE | SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)
        );
        let binding = shared_binding();
        let state = Arc::new(AtomicUsize::new(0));
        let calls = state.clone();
        runtime().unwrap().block_on(async {
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let expected = binding.clone();
            let server = tokio::spawn(async move {
                let (peer, _) = listener.accept().await.unwrap();
                let service = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let expected = expected.clone();
                    let calls = calls.clone();
                    async move {
                        let sequence = calls.fetch_add(1, Ordering::Relaxed);
                        let method = req.method().clone();
                        let path = req.uri().to_string();
                        let mut status = StatusCode::OK;
                        let body = match sequence {
                            0 => {
                                assert_eq!((method, path.as_str()), (Method::GET, "/v1.44/info"));
                                serde_json::json!({"ID":expected.engine_id,"OSType":"linux","Architecture":"x86_64",
                                    "Driver":"overlay2","DriverStatus":[["Backing Filesystem","extfs"]]})
                            }
                            1 | 3 => {
                                assert_eq!(method, Method::GET);
                                assert_eq!(path, format!("/v1.44/images/{}/json", expected.image_id));
                                if sequence == 1 {
                                    status = StatusCode::NOT_FOUND;
                                    serde_json::json!({"message": "image is absent"})
                                } else {
                                    serde_json::json!({"Id":expected.image_id,"Os":"linux","Architecture":"amd64",
                                        "RootFS":{"Type":"layers","Layers":expected.diff_ids},"Config":{},"RepoTags":null})
                                }
                            }
                            2 => {
                                assert_eq!((method, path.as_str()), (Method::POST, "/v1.44/images/load?quiet=1"));
                                assert_eq!(req.headers()[header::CONTENT_TYPE], "application/x-tar");
                                assert_eq!(req.headers()[header::CONTENT_LENGTH], "17");
                                assert_eq!(req.into_body().collect().await.unwrap().to_bytes(), b"exact fd contents".as_slice());
                                serde_json::json!({"stream":format!("Loaded image ID: {}\n", expected.image_id)})
                            }
                            _ => panic!("unexpected Docker request"),
                        };
                        Ok::<_, Infallible>(Response::builder().status(status)
                            .body(Full::new(Bytes::from(body.to_string()))).unwrap())
                    }
                });
                server_http1::Builder::new().serve_connection(TokioIo::new(peer), service).await.unwrap();
            });
            let mut client = Connection::open(&socket).await.unwrap();
            client.identity(&binding.engine_id).await.unwrap();
            assert_eq!(client.observe(&binding).await.unwrap(), Presence::Absent);
            client.ready().await.unwrap();
            client.load(sealed, &binding.image_id).await.unwrap();
            assert_eq!(client.observe(&binding).await.unwrap(), Presence::Present);
            drop(client);
            server.await.unwrap();
        });
        assert_eq!(state.load(Ordering::Relaxed), 4);
    }
}
