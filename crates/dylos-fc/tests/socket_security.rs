use dylos_fc::{Error, FcClient};
use std::os::unix::fs::symlink;
use std::path::Path;
use tokio::net::UnixListener;

fn fd_count() -> std::io::Result<usize> {
    std::fs::read_dir("/proc/self/fd")?.try_fold(0, |count, entry| entry.map(|_| count + 1))
}

async fn refused(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let error = FcClient::new(path)
        .get::<serde_json::Value>("/")
        .await
        .err()
        .ok_or("unexpected successful connection")?;
    match error {
        Error::Connect {
            path: reported,
            method,
            route,
            ..
        } => {
            assert_eq!(reported, path);
            assert_eq!(method, "GET");
            assert_eq!(route, "/");
        }
        other => return Err(format!("expected contextual connect error, got {other:?}").into()),
    }
    Ok(())
}

// One test in its own integration-test process keeps the fd census independent
// of concurrent HTTP tests and their background connection tasks.
#[tokio::test]
async fn refuses_socket_redirection_and_closes_descriptors()
-> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let target_dir = dir.path().join("target");
    std::fs::create_dir(&target_dir)?;
    let target = target_dir.join("firecracker.socket");
    let listener = UnixListener::bind(&target)?;
    let socket = dir.path().join("firecracker.socket");
    // Warm up Tokio's blocking pool before measuring process descriptors.
    refused(&dir.path().join("missing")).await?;
    let before = fd_count()?;

    symlink(&target, &socket)?;
    for _ in 0..16 {
        refused(&socket).await?;
    }
    assert_eq!(fd_count()?, before);
    std::fs::remove_file(&socket)?;

    std::fs::write(&socket, b"not a socket")?;
    for _ in 0..16 {
        refused(&socket).await?;
    }
    assert_eq!(fd_count()?, before);
    std::fs::remove_file(&socket)?;

    let parent = dir.path().join("run");
    symlink(&target_dir, &parent)?;
    for _ in 0..16 {
        refused(&parent.join("firecracker.socket")).await?;
    }
    assert_eq!(fd_count()?, before);

    // A genuine socket with no listener exercises failure after the inode is pinned.
    let stale = dir.path().join("stale.socket");
    drop(UnixListener::bind(&stale)?);
    for _ in 0..16 {
        refused(&stale).await?;
    }
    assert_eq!(fd_count()?, before);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
            .await
            .is_err(),
        "redirect target received a connection"
    );
    Ok(())
}
