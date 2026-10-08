use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::config::{
    BootSource, Drive, InstanceActionInfo, MachineConfiguration, NetworkInterface,
};
use crate::error::{Error, Result};
use crate::snapshot::{SnapshotCreateParams, SnapshotLoadParams, Vm};

pub struct FcClient {
    socket_path: PathBuf,
    timeout: Duration,
}

const RESPONSE_SIZE_LIMIT: usize = 1024 * 1024; // 1 MiB
const FAULT_MESSAGE_LIMIT: usize = 1024; // 1 KiB
const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000); // 1s

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

/// Firecracker resource identifiers (`drive_id`, `iface_id`) must be non-empty and match `[A-Za-z0-9_-]`.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty()
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(Error::InvalidId { id: id.to_string() });
    }
    Ok(())
}

impl FcClient {
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn new(socket_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            timeout: DEFAULT_TIMEOUT,
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
        let res = match timeout(
            self.timeout,
            self.request_inner(method.clone(), route, body),
        )
        .await
        {
            Ok(res) => res,
            Err(_) => Err(Error::Timeout {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
            }),
        };
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
        let limited_body = http_body_util::Limited::new(res.into_body(), RESPONSE_SIZE_LIMIT);
        let body_bytes = limited_body
            .collect()
            .await
            .map_err(|e| {
                if e.downcast_ref::<http_body_util::LengthLimitError>()
                    .is_some()
                {
                    Error::ResponseTooLarge {
                        path: self.socket_path.clone(),
                        method: method.to_string(),
                        route: route.to_string(),
                        limit: RESPONSE_SIZE_LIMIT,
                    }
                } else {
                    Error::BodyRead {
                        path: self.socket_path.clone(),
                        method: method.to_string(),
                        route: route.to_string(),
                        status: status.as_u16(),
                        source: e,
                    }
                }
            })?
            .to_bytes();

        if !status.is_success() {
            let mut fault_message =
                if let Ok(fault) = serde_json::from_slice::<FaultMessage>(&body_bytes) {
                    fault.fault_message
                } else {
                    String::from_utf8_lossy(&body_bytes).into_owned()
                };

            if fault_message.len() > FAULT_MESSAGE_LIMIT {
                fault_message.truncate(FAULT_MESSAGE_LIMIT);
                fault_message.push_str("...");
            }

            return Err(Error::Api {
                path: self.socket_path.clone(),
                method: method.to_string(),
                route: route.to_string(),
                status: status.as_u16(),
                fault_message,
            });
        }

        Self::parse_response(&self.socket_path, &method, route, status, &body_bytes)
    }

    fn parse_response<R: for<'de> Deserialize<'de>>(
        socket_path: &std::path::Path,
        method: &hyper::Method,
        route: &str,
        status: hyper::StatusCode,
        body_bytes: &[u8],
    ) -> Result<Option<R>> {
        if body_bytes.is_empty() {
            return Ok(None);
        }
        let parsed: R = serde_json::from_slice(body_bytes).map_err(|e| Error::Deserialize {
            path: socket_path.to_path_buf(),
            method: method.to_string(),
            route: route.to_string(),
            status: status.as_u16(),
            source: e,
        })?;
        Ok(Some(parsed))
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

    /// Configures the microVM vCPU count, memory, and related machine parameters (`PUT /machine-config`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn put_machine_config(&self, config: &MachineConfiguration) -> Result<()> {
        let _: Option<serde_json::Value> = self.put("/machine-config", config).await?;
        Ok(())
    }

    /// Configures the microVM kernel and boot arguments (`PUT /boot-source`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn put_boot_source(&self, boot_source: &BootSource) -> Result<()> {
        let _: Option<serde_json::Value> = self.put("/boot-source", boot_source).await?;
        Ok(())
    }

    /// Attaches or updates a drive (`PUT /drives/{drive_id}`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidId`] if `drive_id` is empty or contains characters outside `[A-Za-z0-9_-]`.
    /// Also returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn put_drive(&self, drive: &Drive) -> Result<()> {
        validate_id(&drive.drive_id)?;
        let route = format!("/drives/{}", drive.drive_id);
        let _: Option<serde_json::Value> = self.put(&route, drive).await?;
        Ok(())
    }

    /// Attaches or updates a network interface (`PUT /network-interfaces/{iface_id}`).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidId`] if `iface_id` is empty or contains characters outside `[A-Za-z0-9_-]`.
    /// Also returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn put_network_interface(&self, iface: &NetworkInterface) -> Result<()> {
        validate_id(&iface.iface_id)?;
        let route = format!("/network-interfaces/{}", iface.iface_id);
        let _: Option<serde_json::Value> = self.put(&route, iface).await?;
        Ok(())
    }

    /// Boots the microVM (`PUT /actions` with `InstanceStart`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn start_instance(&self) -> Result<()> {
        let body = InstanceActionInfo::instance_start();
        let _: Option<serde_json::Value> = self.put("/actions", &body).await?;
        Ok(())
    }

    /// Pauses the microVM vCPUs (`PATCH /vm` with `Paused`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn pause(&self) -> Result<()> {
        let body = Vm::pause();
        let _: Option<serde_json::Value> = self.patch("/vm", &body).await?;
        Ok(())
    }

    /// Resumes the microVM vCPUs (`PATCH /vm` with `Resumed`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn resume(&self) -> Result<()> {
        let body = Vm::resume();
        let _: Option<serde_json::Value> = self.patch("/vm", &body).await?;
        Ok(())
    }

    /// Creates a snapshot of the microVM state and memory (`PUT /snapshot/create`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn create_snapshot(&self, params: &SnapshotCreateParams) -> Result<()> {
        let _: Option<serde_json::Value> = self.put("/snapshot/create", params).await?;
        Ok(())
    }

    /// Loads a microVM from a snapshot (`PUT /snapshot/load`).
    ///
    /// # Errors
    ///
    /// Returns [`Error`] on connection, HTTP, serialization, or API error.
    pub async fn load_snapshot(&self, params: &SnapshotLoadParams) -> Result<()> {
        let _: Option<serde_json::Value> = self.put("/snapshot/load", params).await?;
        Ok(())
    }
}
