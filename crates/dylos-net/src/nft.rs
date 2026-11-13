use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::info;

use crate::{Error, netns};

// Never search PATH after entering a namespace or accept executable paths from LabSpec.
pub(crate) fn resolve(lab: &str) -> Result<PathBuf, Error> {
    let root_owner = std::fs::metadata("/")
        .map_err(netns::io_err(lab, "inspect root owner", Path::new("/")))?
        .uid();
    for candidate in ["/usr/sbin/nft", "/usr/bin/nft"] {
        let path = match std::fs::canonicalize(candidate) {
            Ok(path) => path,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(netns::io_err(lab, "resolve nft", Path::new(candidate))(
                    error,
                ));
            }
        };
        for component in path.ancestors() {
            let metadata = std::fs::metadata(component).map_err(netns::io_err(
                lab,
                "inspect nft path",
                component,
            ))?;
            if metadata.uid() != root_owner || metadata.mode() & 0o022 != 0 {
                return Err(failure(
                    lab,
                    "nft path must be owned by the filesystem root owner and not group/world-writable",
                ));
            }
        }
        if !path.is_file() {
            return Err(failure(lab, "nft must be a regular file"));
        }
        return Ok(path);
    }
    Err(failure(
        lab,
        "trusted nft executable missing from /usr/sbin and /usr/bin",
    ))
}

pub(crate) fn failure(lab: &str, message: impl Into<String>) -> Error {
    Error::FabricTransition {
        lab: lab.into(),
        step: "peer filtering",
        failures: vec![message.into()],
        rollback_failures: vec![],
    }
}

/// One transaction with bounded diagnostics; deadline failure kills and reaps the child.
pub(crate) async fn batch(lab: &str, executable: &Path, rules: &str) -> Result<(), Error> {
    let started = Instant::now();
    let map_io = || netns::io_err(lab, "execute nft batch", executable);
    let mut child = tokio::process::Command::new(executable)
        .args(["-f", "-"])
        .env_clear()
        .env("LC_ALL", "C")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_io())?;
    let mut input = child
        .stdin
        .take()
        .ok_or_else(|| failure(lab, "nft stdin missing"))?;
    let diagnostic = child
        .stderr
        .take()
        .ok_or_else(|| failure(lab, "nft stderr missing"))?;
    let mut diagnostic = diagnostic.take(65_537);
    let mut bytes = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::try_join!(
            async {
                input.write_all(rules.as_bytes()).await?;
                drop(input);
                Ok::<_, io::Error>(())
            },
            diagnostic.read_to_end(&mut bytes),
            child.wait(),
        )
    })
    .await;
    let status = match result {
        Ok(Ok(((), _, status))) => status,
        failed => {
            // start_kill is harmless for an exited child; wait must still reap it.
            let kill = child.start_kill();
            let reap = child.wait().await;
            return Err(failure(
                lab,
                format!("nft batch incomplete: {failed:?}; kill: {kill:?}; reap: {reap:?}"),
            ));
        }
    };
    info!(
        lab,
        step = "peer filtering",
        duration_ms = started.elapsed().as_secs_f64() * 1000.0,
        success = status.success(),
        "nft batch completed"
    );
    if !status.success() || bytes.len() > 65_536 {
        bytes.truncate(65_536);
        return Err(failure(
            lab,
            format!("nft {status}: {}", String::from_utf8_lossy(&bytes)),
        ));
    }
    Ok(())
}
