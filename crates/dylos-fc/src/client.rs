use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio::net::UnixStream;

use crate::error::{Error, Result};

pub struct FcClient {
    socket_path: PathBuf,
}

#[derive(Deserialize)]
struct FaultMessage {
    fault_message: String,
}

/// Firecracker ignores the host over a Unix socket, but HTTP/1.1 requires one.
const API_AUTHORITY: &str = "localhost";

fn request_uri(route: &str) -> Result<hyper::Uri> {
    if !route.starts_with('/') {
        return Err(Error::InvalidRoute {
            route: route.to_string(),
        });
    }
    format!("http://{API_AUTHORITY}{route}")
        .parse()
        .map_err(|e| Error::InvalidUri {
            route: route.to_string(),
            source: e,
        })
}

impl FcClient {
    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
        }
    }

    #[tracing::instrument(skip(self, body), fields(method = %method, route = %route, duration_ms = tracing::field::Empty))]
    async fn request<B, R>(
        &self,
        method: hyper::Method,
        route: &str,
        body: Option<&B>,
    ) -> Result<Option<R>>
    where
        B: Serialize + ?Sized,
        R: for<'de> Deserialize<'de>,
    {
        let start = std::time::Instant::now();
        let res = self.request_inner(method, route, body).await;
        tracing::Span::current().record(
            "duration_ms",
            u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        );
        res
    }

    async fn request_inner<B, R>(
        &self,
        method: hyper::Method,
        route: &str,
        body: Option<&B>,
    ) -> Result<Option<R>>
    where
        B: Serialize + ?Sized,
        R: for<'de> Deserialize<'de>,
    {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| Error::Connect {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                source: e,
            })?;

        let io = TokioIo::new(stream);
        let (mut sender, conn) = http1::handshake(io).await.map_err(|e| Error::Handshake {
            path: self.socket_path.clone(),
            method: method.to_string(),
            route: route.to_string(),
            source: e,
        })?;

        tokio::spawn(async move {
            if let Err(err) = conn.await {
                tracing::debug!("Connection failed: {:?}", err);
            }
        });

        let body_bytes = match body {
            Some(b) => serde_json::to_vec(b).map_err(|e| Error::Serialize {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                source: e,
            })?,
            None => vec![],
        };
        let req_body = Full::new(Bytes::from(body_bytes));

        let req = Request::builder()
            .method(method.clone())
            .uri(request_uri(route)?)
            .header(hyper::header::ACCEPT, "application/json")
            .header(hyper::header::CONTENT_TYPE, "application/json")
            .body(req_body)
            .map_err(|e| Error::RequestBuilder {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                source: e,
            })?;

        let res = sender.send_request(req).await.map_err(|e| Error::Request {
            path: self.socket_path.clone(),
            method: method.to_string(),
            route: route.to_string(),
            source: e,
        })?;

        let status = res.status();
        let body_bytes = res
            .into_body()
            .collect()
            .await
            .map_err(|e| Error::BodyRead {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                status: status.as_u16(),
                source: e,
            })?
            .to_bytes();

        if !status.is_success() {
            let fault_message =
                if let Ok(fault) = serde_json::from_slice::<FaultMessage>(&body_bytes) {
                    fault.fault_message
                } else {
                    String::from_utf8_lossy(&body_bytes).into_owned()
                };

            return Err(Error::Api {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                status: status.as_u16(),
                fault_message,
            });
        }

        if body_bytes.is_empty() {
            Ok(None)
        } else {
            let parsed: R =
                serde_json::from_slice(&body_bytes).map_err(|e| Error::Deserialize {
                    path: self.socket_path.clone(),
                    method: method.to_string(),
                    route: route.to_string(),
                    status: status.as_u16(),
                    source: e,
                })?;
            Ok(Some(parsed))
        }
    }

    /// `route` starts with `/` (for example `/machine-config`). Returns `Ok(None)` when Firecracker
    /// answers with an empty body, which is the usual `204 No Content` reply to PUT and PATCH.
    ///
    /// # Errors
    ///
    /// Any [`Error`] variant; a non-2xx status gives [`Error::Api`] with Firecracker's `fault_message`.
    pub async fn get<R>(&self, route: &str) -> Result<Option<R>>
    where
        R: for<'de> Deserialize<'de>,
    {
        self.request::<(), R>(hyper::Method::GET, route, None).await
    }

    /// Same contract as [`FcClient::get`], with `body` sent as JSON.
    ///
    /// # Errors
    ///
    /// As for [`FcClient::get`].
    pub async fn put<B, R>(&self, route: &str, body: &B) -> Result<Option<R>>
    where
        B: Serialize + ?Sized,
        R: for<'de> Deserialize<'de>,
    {
        self.request(hyper::Method::PUT, route, Some(body)).await
    }

    /// Same contract as [`FcClient::get`], with `body` sent as JSON.
    ///
    /// # Errors
    ///
    /// As for [`FcClient::get`].
    pub async fn patch<B, R>(&self, route: &str, body: &B) -> Result<Option<R>>
    where
        B: Serialize + ?Sized,
        R: for<'de> Deserialize<'de>,
    {
        self.request(hyper::Method::PATCH, route, Some(body)).await
    }
}
