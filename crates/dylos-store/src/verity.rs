//! Linux fs-verity SHA-256 measurement and publication.
use std::{fs::File, io, os::fd::AsRawFd};

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
