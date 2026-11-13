//! Linux fs-verity SHA-256 measurement and publication.
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    sync::OnceLock,
    time::Instant,
};

const PAGE_SIZE: usize = 4096;
const PROBE_PAGES: u64 = 32 * 1024 * 1024 / 4096;

/// Seals content only after a process-wide sparse-file integrity probe succeeds.
/// Only the first store filesystem can use verity; other devices retain full hashing.
/// Call only after thaw; this can write and read 32 MiB once per process.
/// # Errors
/// Returns errors sealing the real file after a successful probe.
pub fn enable_checked(file: &File, directory: &File) -> io::Result<Option<String>> {
    static SUPPORTED: OnceLock<Option<u64>> = OnceLock::new();
    let device = directory.metadata()?.dev();
    let supported = SUPPORTED.get_or_init(|| {
        let start = Instant::now();
        match probe(directory) {
            Ok(()) => {
                tracing::info!(
                    elapsed_ms = start.elapsed().as_millis(),
                    "fs-verity sparse probe passed"
                );
                Some(device)
            }
            Err(error) => {
                tracing::warn!(%error, elapsed_ms = start.elapsed().as_millis(),
                    "fs-verity sparse probe failed; retaining full content hashing");
                None
            }
        }
    });
    if *supported == Some(device) {
        enable(file)
    } else {
        Ok(None)
    }
}

fn probe_page(index: u64) -> [u8; PAGE_SIZE] {
    let mut page = [0; PAGE_SIZE];
    if Sha256::digest(index.to_le_bytes())[0] % 10 >= 3 {
        for word in page.as_chunks_mut::<8>().0 {
            word.copy_from_slice(&index.to_le_bytes());
        }
    }
    page
}

/// Validates the complete 32 MiB sparse integrity probe after cache eviction.
/// # Errors
/// Propagates read errors (including EIO), truncation, extra bytes and mismatches.
pub fn verify_probe(mut reader: impl Read) -> io::Result<()> {
    let mut page = [0; PAGE_SIZE];
    for index in 0..PROBE_PAGES {
        reader.read_exact(&mut page)?;
        if page != probe_page(index) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "fs-verity sparse probe content mismatch",
            ));
        }
    }
    if reader.read(&mut page[..1])? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "fs-verity sparse probe size mismatch",
        ));
    }
    Ok(())
}

fn probe(directory: &File) -> io::Result<()> {
    use nix::{
        fcntl::{OFlag, openat},
        sys::stat::Mode,
        unistd::{UnlinkatFlags, unlinkat},
    };
    struct Cleanup<'a>(&'a File, &'a str);
    impl Drop for Cleanup<'_> {
        fn drop(&mut self) {
            if let Err(error) = unlinkat(self.0, self.1, UnlinkatFlags::NoRemoveDir) {
                tracing::warn!(%error, "failed to remove fs-verity probe");
            }
        }
    }
    let name = format!(".verity-probe-{}", std::process::id());
    let output = File::from(openat(
        directory,
        name.as_str(),
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )?);
    let _cleanup = Cleanup(directory, &name);
    let mut output = output;
    // Match capture: contiguous nonzero page runs, split at 1 MiB boundaries.
    let mut run = Vec::with_capacity(1024 * 1024);
    for chunk in (0..PROBE_PAGES).step_by(256) {
        for index in chunk..chunk + 256 {
            let page = probe_page(index);
            if page == [0; PAGE_SIZE] {
                output.write_all(&run)?;
                run.clear();
                output.seek(SeekFrom::Current(4096))?;
            } else {
                run.extend_from_slice(&page);
            }
        }
        output.write_all(&run)?;
        run.clear();
    }
    output.set_len(PROBE_PAGES * 4096)?;
    output.sync_all()?;
    drop(output);
    let input = File::from(openat(
        directory,
        name.as_str(),
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    )?);
    if enable(&input)?.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "fs-verity unavailable on store filesystem",
        ));
    }
    // SAFETY: the descriptor remains live; posix_fadvise does not access user memory.
    let result = unsafe { libc::posix_fadvise(input.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    verify_probe(input)
}

#[repr(C)]
#[derive(Default)]
struct Enable {
    version: u32,
    algorithm: u32,
    block_size: u32,
    salt_size: u32,
    salt_ptr: u64,
    signature_size: u32,
    reserved1: u32,
    signature_ptr: u64,
    reserved2: [u64; 11],
}

#[repr(C)]
struct Measurement {
    algorithm: u16,
    size: u16,
    digest: [u8; 32],
}

/// Seals a captured file, or returns `None` when fs-verity is unsupported.
///
/// # Errors
/// Returns enablement errors other than unsupported kernel/filesystem ioctls,
/// and any failure to measure a successfully sealed file.
pub fn enable(file: &File) -> io::Result<Option<String>> {
    let arg = Enable {
        version: 1,
        algorithm: 1,
        block_size: 4096,
        ..Enable::default()
    };
    // SAFETY: arg matches Linux fsverity_enable_arg (128 bytes), contains no
    // userspace pointers, and stays live along with the descriptor during ioctl.
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            nix::request_code_write!(b'f', 133, size_of::<Enable>()),
            &raw const arg,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::ENOSYS) => {
                tracing::debug!(%error, "fs-verity unsupported; retaining full content hashing");
                return Ok(None);
            }
            Some(libc::EEXIST) => {}
            _ => return Err(error),
        }
    }
    measure(file).map(Some)
}

/// Measures the kernel's SHA-256 fs-verity digest without reading file content.
///
/// # Errors
/// Refuses unsealed files, unsupported ioctls, and unexpected digest algorithms.
pub fn measure(file: &File) -> io::Result<String> {
    let mut measurement = Measurement {
        algorithm: 0,
        size: 32,
        digest: [0; 32],
    };
    // SAFETY: Measurement starts with the Linux fsverity_digest header followed
    // by the advertised 32-byte output buffer. ioctl's encoded size is the
    // four-byte flexible-array header, not the size of our complete buffer.
    let result = unsafe {
        libc::ioctl(
            file.as_raw_fd(),
            nix::request_code_readwrite!(b'f', 134, 4),
            &raw mut measurement,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    if measurement.algorithm != 1 || measurement.size != 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected a SHA-256 fs-verity digest",
        ));
    }
    Ok(dylos_core::manifest::sha256_hex(&measurement.digest))
}
