#![allow(clippy::unwrap_used)]
use dylos_runtime::agent::{AgentError, resync};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn rejected(bytes: Vec<u8>) -> AgentError {
    let (mut host, mut guest) = tokio::io::duplex(8192);
    let peer = tokio::spawn(async move {
        let mut request = [0; 256];
        assert!(guest.read(&mut request).await.unwrap() > 0);
        guest.write_all(&bytes).await.unwrap();
    });
    let error = resync(&mut host, Duration::from_secs(1)).await.unwrap_err();
    peer.await.unwrap();
    error
}

#[tokio::test]
async fn oversized_line_is_rejected_without_waiting_for_newline() {
    assert!(matches!(rejected(vec![b'x'; 4096]).await, AgentError::Line));
}

#[tokio::test]
async fn invalid_utf8_and_json_are_rejected() {
    assert!(matches!(
        rejected(vec![255, b'\n']).await,
        AgentError::Utf8(_)
    ));
    assert!(matches!(
        rejected(b"not json\n".to_vec()).await,
        AgentError::Protocol(_)
    ));
}

#[tokio::test]
async fn slow_peer_has_a_bounded_exchange() {
    let (mut host, mut guest) = tokio::io::duplex(8192);
    guest.write_all(b"{").await.unwrap();
    assert!(matches!(
        resync(&mut host, Duration::from_millis(20)).await,
        Err(AgentError::Timeout)
    ));
}

#[tokio::test]
async fn resync_then_health_uses_shared_protocol() {
    let (mut host, guest) = tokio::io::duplex(8192);
    let peer = tokio::spawn(async move {
        dylos_agent::agent::serve(guest, &FakeClock).await.unwrap();
    });
    assert!(matches!(
        resync(&mut host, Duration::from_secs(1)).await.unwrap(),
        dylos_agent::protocol::ReplyBody::Health {
            wall_clock_unix_ns: 123,
            ..
        }
    ));
    drop(host);
    peer.await.unwrap();
}

struct FakeClock;
impl dylos_agent::clock::GuestClock for FakeClock {
    fn set_unix_ns(&self, _: u64) -> Result<(), dylos_agent::clock::ClockError> {
        Ok(())
    }
    fn now_unix_ns(&self) -> Result<u64, dylos_agent::clock::ClockError> {
        Ok(123)
    }
    fn uptime(&self) -> Result<Duration, dylos_agent::clock::ClockError> {
        Ok(Duration::ZERO)
    }
}

struct FakeEntropy(std::sync::Mutex<Vec<[u8; 32]>>);
impl dylos_agent::entropy::GuestEntropy for FakeEntropy {
    fn mix(&self, bytes: [u8; 32]) -> std::io::Result<()> {
        self.0.lock().unwrap().push(bytes);
        Ok(())
    }
}

#[tokio::test]
async fn restored_guests_receive_independent_host_entropy_before_health() {
    let entropy = std::sync::Arc::new(FakeEntropy(std::sync::Mutex::new(Vec::new())));
    for _ in 0..2 {
        let (mut host, guest) = tokio::io::duplex(8192);
        let source = entropy.clone();
        let peer = tokio::spawn(async move {
            dylos_agent::agent::serve_with_entropy(guest, &FakeClock, source)
                .await
                .unwrap();
        });
        dylos_runtime::agent::inject_entropy(&mut host, Duration::from_secs(1))
            .await
            .unwrap();
        resync(&mut host, Duration::from_secs(1)).await.unwrap();
        drop(host);
        peer.await.unwrap();
    }
    let injected = entropy.0.lock().unwrap();
    assert_eq!(injected.len(), 2);
    assert_ne!(injected[0], injected[1]);
}

struct FailedEntropy;
impl dylos_agent::entropy::GuestEntropy for FailedEntropy {
    fn mix(&self, _: [u8; 32]) -> std::io::Result<()> {
        Err(std::io::ErrorKind::PermissionDenied.into())
    }
}

#[tokio::test]
async fn failed_entropy_injection_is_not_acknowledged() {
    let (mut host, guest) = tokio::io::duplex(8192);
    let peer = tokio::spawn(async move {
        dylos_agent::agent::serve_with_entropy(
            guest,
            &FakeClock,
            std::sync::Arc::new(FailedEntropy),
        )
        .await
        .unwrap();
    });
    assert!(matches!(
        dylos_runtime::agent::inject_entropy(&mut host, Duration::from_secs(1)).await,
        Err(AgentError::Unexpected)
    ));
    drop(host);
    peer.await.unwrap();
}
