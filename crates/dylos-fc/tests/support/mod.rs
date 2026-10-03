use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;
use tokio::sync::Notify;

pub struct FakeServer {
    socket_path: PathBuf,
    history: Arc<Mutex<Vec<RequestRecord>>>,
    reply: Arc<Mutex<ReplyConfig>>,
    shutdown: Arc<Notify>,
    server_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone, Debug)]
pub struct RequestRecord {
    pub method: String,
    pub path: String,
    // Not every test binary sharing this module reads it.
    #[allow(dead_code)]
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Clone)]
pub struct ReplyConfig {
    pub status: StatusCode,
    pub body: Vec<u8>,
}

impl FakeServer {
    pub async fn new(socket_path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let socket_path = socket_path.into();
        let _ = tokio::fs::remove_file(&socket_path).await;

        let listener = UnixListener::bind(&socket_path)?;

        let history = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new(ReplyConfig {
            status: StatusCode::NO_CONTENT,
            body: vec![],
        }));
        let shutdown = Arc::new(Notify::new());

        let h_clone = Arc::clone(&history);
        let r_clone = Arc::clone(&reply);
        let s_clone = Arc::clone(&shutdown);

        let server_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    () = s_clone.notified() => break,
                    accept_res = listener.accept() => {
                        if let Ok((stream, _)) = accept_res {
                            let h = Arc::clone(&h_clone);
                            let r = Arc::clone(&r_clone);

                            tokio::spawn(async move {
                                let io = hyper_util::rt::TokioIo::new(stream);
                                let svc = service_fn(move |req: Request<Incoming>| {
                                    let h = Arc::clone(&h);
                                    let r = Arc::clone(&r);
                                    async move {
                                        let method = req.method().to_string();
                                        let path = req.uri().path().to_string();
                                        let content_type = req
                                            .headers()
                                            .get(hyper::header::CONTENT_TYPE)
                                            .and_then(|v| v.to_str().ok())
                                            .map(str::to_string);

                                        // Handle reading the body without unwrap
                                        let request_body = match req.into_body().collect().await {
                                            Ok(b) => b.to_bytes().to_vec(),
                                            Err(_) => vec![],
                                        };

                                        if let Ok(mut hist) = h.lock() {
                                            hist.push(RequestRecord {
                                                method,
                                                path,
                                                content_type,
                                                body: request_body,
                                            });
                                        }

                                        let mut rc = ReplyConfig { status: StatusCode::NO_CONTENT, body: vec![] };
                                        if let Ok(reply_lock) = r.lock() {
                                            rc = reply_lock.clone();
                                        }

                                        let response_body = Full::new(Bytes::from(rc.body));
                                        let res = Response::builder()
                                            .status(rc.status)
                                            .header(hyper::header::CONTENT_TYPE, "application/json")
                                            .body(response_body)
                                            .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));

                                        Ok::<_, Infallible>(res)
                                    }
                                });

                                if let Err(err) = http1::Builder::new().serve_connection(io, svc).await {
                                    tracing::debug!("Server connection error: {:?}", err);
                                }
                            });
                        }
                    }
                }
            }
        });

        Ok(Self {
            socket_path,
            history,
            reply,
            shutdown,
            server_task: Some(server_task),
        })
    }

    pub fn set_reply(&self, status: StatusCode, body: Vec<u8>) {
        if let Ok(mut r) = self.reply.lock() {
            r.status = status;
            r.body = body;
        }
    }

    pub fn history(&self) -> Vec<RequestRecord> {
        if let Ok(h) = self.history.lock() {
            h.clone()
        } else {
            vec![]
        }
    }

    pub async fn shutdown(mut self) {
        self.shutdown.notify_waiters();
        if let Some(task) = self.server_task.take() {
            let _ = task.await;
        }
        let _ = tokio::fs::remove_file(&self.socket_path).await;
    }
}
