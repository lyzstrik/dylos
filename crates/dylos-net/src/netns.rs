use std::fs::File;
use std::io;
use std::path::Path;

use nix::errno::Errno;
use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, setns, unshare};

use crate::Error;

/// Where iproute2 (`ip netns`) and the Firecracker jailer (`--netns`) expect named netns.
pub const DEFAULT_NETNS_DIR: &str = "/run/netns";

pub(crate) fn io_err(
    lab: &str,
    op: &'static str,
    target: &Path,
) -> impl FnOnce(io::Error) -> Error {
    let (lab, target) = (lab.to_owned(), target.display().to_string());
    move |source| Error::Io {
        lab,
        op,
        target,
        source,
    }
}

/// Runs `f` on a new OS thread and waits for it without blocking.
///
/// `unshare(CLONE_NEWNET)` and `setns` only move the calling thread. Running them on a tokio
/// worker or a pooled `spawn_blocking` thread would leave that thread in the lab netns for
/// unrelated later tasks, so they only ever run on a thread that exits right afterwards.
pub(crate) async fn on_fresh_thread<T, F>(lab: &str, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Error> + Send + 'static,
{
    let owned = lab.to_owned();
    tokio::task::spawn_blocking(move || on_fresh_thread_blocking(&owned, f))
        .await
        .map_err(|_| Error::WorkerPanicked {
            lab: lab.to_owned(),
        })?
}

pub(crate) fn on_fresh_thread_blocking<T, F>(lab: &str, f: F) -> Result<T, Error>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, Error> + Send + 'static,
{
    std::thread::Builder::new()
        .name("dylos-netns".into())
        .spawn(f)
        .map_err(io_err(lab, "spawn netns thread", Path::new("")))?
        .join()
        .map_err(|_| Error::WorkerPanicked {
            lab: lab.to_owned(),
        })?
}

/// Creates a new netns, moves the calling thread into it and pins it at `path` with a bind
/// mount, like `ip netns add`. The netns then outlives the thread until [`remove`].
pub(crate) fn create_and_enter(lab: &str, path: &Path) -> Result<(), Error> {
    File::options()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|source| match source.kind() {
            io::ErrorKind::AlreadyExists => Error::NetnsExists {
                lab: lab.to_owned(),
                path: path.to_owned(),
            },
            _ => io_err(lab, "create netns file", path)(source),
        })?;
    unshare(CloneFlags::CLONE_NEWNET).map_err(|e| io_err(lab, "unshare netns", path)(e.into()))?;
    mount(
        Some("/proc/thread-self/ns/net"),
        path,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(|e| io_err(lab, "bind-mount netns", path)(e.into()))
}

/// Moves the calling thread into the netns pinned at `path`. Returns `false` when there is
/// nothing to enter: no file, or a file left by a creation that failed before the bind mount.
pub(crate) fn enter(lab: &str, path: &Path) -> Result<bool, Error> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err(lab, "open netns", path)(e)),
    };
    match setns(&file, CloneFlags::CLONE_NEWNET) {
        Ok(()) => Ok(true),
        Err(Errno::EINVAL) => Ok(false),
        Err(e) => Err(io_err(lab, "enter netns", path)(e.into())),
    }
}

/// Unpins the netns at `path`; the kernel destroys it, with every device inside, once no
/// process, thread or open descriptor references it any more. Succeeds if already removed.
pub(crate) fn remove(lab: &str, path: &Path) -> Result<(), Error> {
    match umount2(path, MntFlags::MNT_DETACH) {
        Ok(()) | Err(Errno::EINVAL | Errno::ENOENT) => {}
        Err(e) => return Err(io_err(lab, "unmount netns", path)(e.into())),
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            Err(io_err(lab, "remove netns file", path)(e))
        }
        _ => Ok(()),
    }
}
