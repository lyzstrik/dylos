use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::net::UnixListener;

pub struct FakeServer {
    socket_path: PathBuf,
    history: Arc<Mutex<Vec<RequestRecord>>>,
    reply: Arc<Mutex<ReplyConfig>>,
    shutdown_tx: tokio::sync::watch::Sender<bool>,
    server_task: Option<tokio::task::JoinHandle<()>>,
    conn_tasks: Arc<Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
    pub fn new(socket_path: impl Into<PathBuf>) -> Result<Self, std::io::Error> {
        let socket_path = socket_path.into();
        let _ = std::fs::remove_file(&socket_path);

        let listener = UnixListener::bind(&socket_path)?;

        let history = Arc::new(Mutex::new(Vec::new()));
        let reply = Arc::new(Mutex::new(ReplyConfig {
            status: StatusCode::NO_CONTENT,
            body: vec![],
        }));
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
        let conn_tasks = Arc::new(Mutex::new(Vec::new()));

        let h_clone = Arc::clone(&history);
        let r_clone = Arc::clone(&reply);
        let c_clone = Arc::clone(&conn_tasks);

        let server_task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => break,
                    accept_res = listener.accept() => {
                        if let Ok((stream, _)) = accept_res {
                            let h = Arc::clone(&h_clone);
                            let r = Arc::clone(&r_clone);

                            let conn_task = tokio::spawn(async move {
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

                            if let Ok(mut tasks) = c_clone.lock() {
                                tasks.push(conn_task);
                            }
                        }
                    }
                }
            }
        });

        Ok(Self {
            socket_path,
            history,
            reply,
            shutdown_tx,
            server_task: Some(server_task),
            conn_tasks,
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
        let _ = self.shutdown_tx.send(true);
        let mut tasks = vec![];
        if let Ok(mut c) = self.conn_tasks.lock() {
            tasks.extend(c.drain(..));
        }
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        if let Some(task) = self.server_task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for FakeServer {
    fn drop(&mut self) {
        let _ = self.shutdown_tx.send(true);
        if let Some(task) = self.server_task.take() {
            task.abort();
        }
        if let Ok(mut tasks) = self.conn_tasks.lock() {
            for task in tasks.drain(..) {
                task.abort();
            }
        }
        if let Err(e) = std::fs::remove_file(&self.socket_path) {
            #[allow(clippy::collapsible_if)]
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::debug!("FakeServer drop: failed to remove socket file: {}", e);
            }
        }
    }
}
