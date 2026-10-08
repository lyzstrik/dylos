//! The only `unsafe` code of the crate: TAP creation through the `/dev/net/tun` ioctls.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::AsRawFd;

nix::ioctl_readwrite_bad!(tun_set_iff, libc::TUNSETIFF, IfReq);
nix::ioctl_write_int_bad!(tun_set_owner, libc::TUNSETOWNER);
nix::ioctl_write_int_bad!(tun_set_group, libc::TUNSETGROUP);
nix::ioctl_write_int_bad!(tun_set_persist, libc::TUNSETPERSIST);

/// `struct ifreq` as read by TUNSETIFF: the name, then `ifr_flags` at the start of the union,
/// padded to the kernel's size so the kernel never reads or writes past it.
#[repr(C)]
struct IfReq {
    name: [u8; libc::IFNAMSIZ],
    flags: libc::c_short,
    pad: [u8; 22],
}

const _: () = assert!(size_of::<IfReq>() == size_of::<libc::ifreq>());

/// Creates a persistent TAP named `name` in the netns of the calling thread.
///
/// `/dev/net/tun` binds the device to the netns of the thread that opens it, so the caller must
/// already be inside the lab netns. The flags match the ones Firecracker passes when it attaches
/// to the TAP; a later TUNSETIFF with a different TUN/TAP type or queue mode would be refused.
pub(crate) fn create_persistent(name: &str, user_id: u32, group_id: u32) -> io::Result<()> {
    if name.len() >= libc::IFNAMSIZ {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    let mut req = IfReq {
        name: [0; libc::IFNAMSIZ],
        #[allow(clippy::cast_possible_truncation)] // the three flags fit in the low 15 bits
        flags: (libc::IFF_TAP | libc::IFF_NO_PI | libc::IFF_VNET_HDR) as libc::c_short,
        pad: [0; 22],
    };
    req.name[..name.len()].copy_from_slice(name.as_bytes());
    let tun = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/net/tun")?;
    // SAFETY: `tun` is an open `/dev/net/tun` descriptor for the whole call. `req` is a live,
    // fully initialised buffer of exactly `sizeof(struct ifreq)` bytes (checked at compile time)
    // with a NUL-terminated name (length checked above). The kernel copies that many bytes in and,
    // on success, back out; nothing keeps the pointer after the call returns.
    unsafe { tun_set_iff(tun.as_raw_fd(), &raw mut req) }?;
    // SAFETY: `tun` owns a valid descriptor attached to the TAP for the whole call.
    // TUNSETOWNER takes the uid integer by value, so no memory is shared with the kernel.
    unsafe { tun_set_owner(tun.as_raw_fd(), i32::from_ne_bytes(user_id.to_ne_bytes())) }?;
    // SAFETY: `tun` owns a valid descriptor attached to the TAP for the whole call.
    // TUNSETGROUP takes the gid integer by value, so no memory is shared with the kernel.
    unsafe { tun_set_group(tun.as_raw_fd(), i32::from_ne_bytes(group_id.to_ne_bytes())) }?;
    // SAFETY: same open descriptor, now attached to the TAP. TUNSETPERSIST takes its argument by
    // value, so no memory is shared with the kernel.
    unsafe { tun_set_persist(tun.as_raw_fd(), 1) }?;
    Ok(())
}
