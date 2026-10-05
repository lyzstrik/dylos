#![allow(clippy::unwrap_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dylos_agent::agent::{Backoff, run, serve};
use dylos_agent::clock::{ClockError, GuestClock};
use dylos_agent::protocol::{Reply, ReplyBody, Request, RequestBody};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

const TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct FakeClock {
    now_ns: AtomicU64,
    sets: Mutex<Vec<u64>>,
    fail_set: bool,
}

impl GuestClock for FakeClock {
    fn set_unix_ns(&self, unix_ns: u64) -> Result<(), ClockError> {
        if self.fail_set {
            return Err(ClockError {
                operation: "fake_settime",
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
        Ok(Duration::from_millis(1500))
    }
}

struct Host {
    reader: BufReader<tokio::io::ReadHalf<DuplexStream>>,
    writer: tokio::io::WriteHalf<DuplexStream>,
}

impl Host {
    fn new(stream: DuplexStream) -> Self {
        let (read, writer) = tokio::io::split(stream);
        Self {
            reader: BufReader::new(read),
            writer,
        }
    }

    async fn send_raw(&mut self, line: &str) -> Reply {
        self.writer.write_all(line.as_bytes()).await.unwrap();
        let mut reply = String::new();
        timeout(TIMEOUT, self.reader.read_line(&mut reply))
            .await
            .unwrap()
            .unwrap();
        Reply::from_line(&reply).unwrap()
    }

    async fn call(&mut self, body: RequestBody) -> ReplyBody {
        self.send_raw(&Request::new(body).to_line().unwrap())
            .await
            .body
    }
}

#[tokio::test]
async fn resync_sets_the_clock() {
    let clock = FakeClock::default();
    let (agent_side, host_side) = tokio::io::duplex(1024);
    let mut host = Host::new(host_side);
    let session = async {
        let reply = host
            .call(RequestBody::Resync {
                unix_time_ns: 1_700_000_000_000_000_001,
            })
            .await;
        assert_eq!(reply, ReplyBody::Ok);
        drop(host);
    };
    let (served, ()) = timeout(TIMEOUT, async {
        tokio::join!(serve(agent_side, &clock), session)
    })
    .await
    .unwrap();
    served.unwrap();
    assert_eq!(*clock.sets.lock().unwrap(), [1_700_000_000_000_000_001]);
}

#[tokio::test]
async fn failed_resync_replies_with_an_error() {
    let clock = FakeClock {
        fail_set: true,
        ..FakeClock::default()
    };
    let (agent_side, host_side) = tokio::io::duplex(1024);
    let mut host = Host::new(host_side);
    let session = async {
        let reply = host.call(RequestBody::Resync { unix_time_ns: 1 }).await;
        assert!(matches!(reply, ReplyBody::Error { message } if message.contains("fake_settime")));
        drop(host);
    };
    let (served, ()) = timeout(TIMEOUT, async {
        tokio::join!(serve(agent_side, &clock), session)
    })
    .await
    .unwrap();
    served.unwrap();
}

#[tokio::test]
async fn health_reports_uptime_and_wall_clock() {
    let clock = FakeClock::default();
    clock.now_ns.store(99, Ordering::SeqCst);
    let (agent_side, host_side) = tokio::io::duplex(1024);
    let mut host = Host::new(host_side);
    let session = async {
        let reply = host.call(RequestBody::Health).await;
        assert!(matches!(
            reply,
            ReplyBody::Health {
                uptime_ms: 1500,
                wall_clock_unix_ns: 99,
                ..
            }
        ));
        drop(host);
    };
    let (served, ()) = timeout(TIMEOUT, async {
        tokio::join!(serve(agent_side, &clock), session)
    })
    .await
    .unwrap();
    served.unwrap();
}

#[tokio::test]
async fn bad_requests_get_an_error_and_keep_the_connection() {
    let clock = FakeClock::default();
    let (agent_side, host_side) = tokio::io::duplex(1024);
    let mut host = Host::new(host_side);
    let session = async {
        for line in ["{\"v\":9,\"type\":\"health\"}\n", "garbage\n"] {
            assert!(matches!(
                host.send_raw(line).await.body,
                ReplyBody::Error { .. }
            ));
        }
        assert!(matches!(
            host.call(RequestBody::Health).await,
            ReplyBody::Health { .. }
        ));
        drop(host);
    };
    let (served, ()) = timeout(TIMEOUT, async {
        tokio::join!(serve(agent_side, &clock), session)
    })
    .await
    .unwrap();
    served.unwrap();
}

#[tokio::test]
async fn reconnects_after_the_peer_drops_the_stream() {
    let (streams_tx, streams_rx) = mpsc::unbounded_channel::<DuplexStream>();
    let streams_rx = std::sync::Arc::new(tokio::sync::Mutex::new(streams_rx));
    let connect_count = std::sync::Arc::new(AtomicU64::new(0));
    let counter = connect_count.clone();
    let agent =
        tokio::spawn(async move {
            let clock = FakeClock::default();
            let backoff = Backoff {
                initial: Duration::from_millis(1),
                max: Duration::from_millis(4),
            };
            run(
                || {
                    counter.fetch_add(1, Ordering::SeqCst);
                    let streams_rx = streams_rx.clone();
                    async move {
                        streams_rx.lock().await.recv().await.ok_or_else(|| {
                            std::io::Error::from(std::io::ErrorKind::ConnectionRefused)
                        })
                    }
                },
                &clock,
                backoff,
            )
            .await
        });

    for _ in 0..3 {
        let (agent_side, host_side) = tokio::io::duplex(1024);
        streams_tx.send(agent_side).unwrap();
        let mut host = Host::new(host_side);
        assert!(matches!(
            host.call(RequestBody::Health).await,
            ReplyBody::Health { .. }
        ));
        // Dropping the host side is what a snapshot restore looks like to the agent.
        drop(host);
    }
    assert!(connect_count.load(Ordering::SeqCst) >= 3);
    agent.abort();
}
