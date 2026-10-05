#![allow(clippy::unwrap_used, clippy::panic)]
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dylos_agent::agent::{Backoff, run, serve};
use dylos_agent::clock::{ClockError, GuestClock};
use dylos_agent::protocol::{Reply, ReplyBody, Request, RequestBody};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct MockClock {
    now_ns: AtomicU64,
    uptime_ms: AtomicU64,
    fail_set: std::sync::atomic::AtomicBool,
    sets: Mutex<Vec<u64>>,
}

impl GuestClock for MockClock {
    fn set_unix_ns(&self, unix_ns: u64) -> Result<(), ClockError> {
        if self.fail_set.load(Ordering::SeqCst) {
            return Err(ClockError {
                operation: "mock_settime",
                source: std::io::Error::from(std::io::ErrorKind::PermissionDenied),
            });
        }
        self.now_ns.store(unix_ns, Ordering::SeqCst);
        self.sets.lock().unwrap().push(unix_ns);
        Ok(())
    }

    fn now_unix_ns(&self) -> Result<u64, ClockError> {
        Ok(self.now_ns.load(Ordering::SeqCst))
    }

    fn uptime(&self) -> Result<Duration, ClockError> {
        Ok(Duration::from_millis(self.uptime_ms.load(Ordering::SeqCst)))
    }
}

struct TestClient {
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    writer: tokio::io::WriteHalf<DuplexStream>,
}

impl TestClient {
    fn new(stream: DuplexStream) -> Self {
        let (read, writer) = tokio::io::split(stream);
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn send_raw(&mut self, line: &str) -> String {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        self.writer.flush().await.unwrap();
        let mut reply = String::new();
        let read = timeout(TIMEOUT, self.reader.read_line(&mut reply))
            .await
            .unwrap()
            .unwrap();
        if read == 0 {
            return String::new();
        }
        reply
    }

    async fn call_raw(&mut self, line: &str) -> Reply {
        let raw = self.send_raw(line).await;
        assert!(!raw.is_empty(), "connection closed unexpectedly");
        Reply::from_line(&raw).unwrap()
    }

    async fn call(&mut self, body: RequestBody) -> ReplyBody {
        let req = Request::new(body);
        let raw = self.send_raw(&req.to_line().unwrap()).await;
        assert!(!raw.is_empty(), "connection closed unexpectedly");
        Reply::from_line(&raw).unwrap().body
    }
}

#[tokio::test]
async fn test_protocol_rejection_keeps_connection() {
    let clock = Arc::new(MockClock::default());
    let (agent_stream, host_stream) = tokio::io::duplex(4096);
    let mut client = TestClient::new(host_stream);

    let server = tokio::spawn({
        let clock = clock.clone();
        async move { serve(agent_stream, clock.as_ref()).await }
    });

    let reply = client.call_raw("{\"type\":\"health\"}\n").await;
    assert!(matches!(reply.body, ReplyBody::Error { .. }));

    let reply = client.call_raw("{\"v\":2,\"type\":\"health\"}\n").await;
    assert!(matches!(reply.body, ReplyBody::Error { .. }));

    let reply = client.call_raw("{{{\n").await;
    assert!(matches!(reply.body, ReplyBody::Error { .. }));

    let reply = client.call(RequestBody::Health).await;
    assert!(matches!(reply, ReplyBody::Health { .. }));

    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_oversized_line_closes_connection() {
    let clock = Arc::new(MockClock::default());
    let (agent_stream, host_stream) = tokio::io::duplex(8192);
    let mut client = TestClient::new(host_stream);

    let server = tokio::spawn({
        let clock = clock.clone();
        async move { serve(agent_stream, clock.as_ref()).await }
    });

    let too_long = "a".repeat(5000) + "\n";
    client.writer.write_all(too_long.as_bytes()).await.unwrap();
    client.writer.flush().await.unwrap();

    let result = server.await.unwrap();
    assert!(result.is_err());
    assert_eq!(
        result.unwrap_err().to_string(),
        "request line exceeds 4096 bytes"
    );
}

#[tokio::test]
async fn test_health_returns_data() {
    let clock = Arc::new(MockClock::default());
    clock.uptime_ms.store(1234, Ordering::SeqCst);
    clock.now_ns.store(5678, Ordering::SeqCst);

    let (agent_stream, host_stream) = tokio::io::duplex(1024);
    let mut client = TestClient::new(host_stream);

    let server = tokio::spawn({
        let clock = clock.clone();
        async move { serve(agent_stream, clock.as_ref()).await }
    });

    let reply = client.call(RequestBody::Health).await;
    if let ReplyBody::Health {
        uptime_ms,
        wall_clock_unix_ns,
        ..
    } = reply
    {
        assert_eq!(uptime_ms, 1234);
        assert_eq!(wall_clock_unix_ns, 5678);
    } else {
        panic!("expected health reply");
    }

    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_resync_success() {
    let clock = Arc::new(MockClock::default());
    let (agent_stream, host_stream) = tokio::io::duplex(1024);
    let mut client = TestClient::new(host_stream);

    let server = tokio::spawn({
        let clock = clock.clone();
        async move { serve(agent_stream, clock.as_ref()).await }
    });

    let reply = client.call(RequestBody::Resync { unix_time_ns: 42 }).await;
    assert_eq!(reply, ReplyBody::Ok);
    assert_eq!(clock.now_ns.load(Ordering::SeqCst), 42);

    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_resync_failure() {
    let clock = Arc::new(MockClock::default());
    clock.fail_set.store(true, Ordering::SeqCst);
    let (agent_stream, host_stream) = tokio::io::duplex(1024);
    let mut client = TestClient::new(host_stream);

    let server = tokio::spawn({
        let clock = clock.clone();
        async move { serve(agent_stream, clock.as_ref()).await }
    });

    let reply = client.call(RequestBody::Resync { unix_time_ns: 42 }).await;
    assert!(matches!(reply, ReplyBody::Error { .. }));
    assert_eq!(clock.now_ns.load(Ordering::SeqCst), 0);

    drop(client);
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_reconnection_loop_after_disconnect() {
    let clock = Arc::new(MockClock::default());
    let (tx, rx) = mpsc::unbounded_channel::<DuplexStream>();
    let rx = Arc::new(tokio::sync::Mutex::new(rx));

    let connects = Arc::new(AtomicU32::new(0));
    let connects_clone = connects.clone();

    let backoff = Backoff {
        initial: Duration::from_millis(1),
        max: Duration::from_millis(5),
    };

    let agent =
        tokio::spawn(async move {
            run(
                || {
                    connects_clone.fetch_add(1, Ordering::SeqCst);
                    let rx_clone = rx.clone();
                    async move {
                        rx_clone.lock().await.recv().await.ok_or_else(|| {
                            std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
                        })
                    }
                },
                clock.as_ref(),
                backoff,
            )
            .await
        });

    let (agent_stream, host_stream) = tokio::io::duplex(1024);
    tx.send(agent_stream).unwrap();
    let mut client = TestClient::new(host_stream);
    assert!(matches!(
        client.call(RequestBody::Health).await,
        ReplyBody::Health { .. }
    ));
    drop(client);

    tokio::time::sleep(Duration::from_millis(50)).await;

    let (agent_stream, host_stream) = tokio::io::duplex(1024);
    tx.send(agent_stream).unwrap();
    let mut client = TestClient::new(host_stream);
    assert!(matches!(
        client.call(RequestBody::Health).await,
        ReplyBody::Health { .. }
    ));
    drop(client);

    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(connects.load(Ordering::SeqCst) >= 3);
    agent.abort();
}

#[test]
fn test_backoff_math() {
    let backoff = Backoff {
        initial: Duration::from_millis(100),
        max: Duration::from_millis(500),
    };

    let delay1 = backoff.next(backoff.initial);
    assert_eq!(delay1, Duration::from_millis(200));

    let delay2 = backoff.next(delay1);
    assert_eq!(delay2, Duration::from_millis(400));

    let delay3 = backoff.next(delay2);
    assert_eq!(delay3, Duration::from_millis(500));

    let delay4 = backoff.next(delay3);
    assert_eq!(delay4, Duration::from_millis(500));
}
