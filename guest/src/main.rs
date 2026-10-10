//! The guest agent: the first process of a session's microvm.
//!
//! It is started from the initramfs. It layers the session's disk over the
//! shared read-only root filesystem, connects to the factory over vsock, runs
//! the setup command and then the harness with the harness's stdio bridged to
//! the factory, and forwards the guest's outbound TCP connections to the
//! factory's proxy, which is the guest's only way out. When the harness ends
//! it shuts the machine down.
//!
//! Only a Linux microvm runs it; elsewhere it builds to a stub that refuses.

#[cfg(target_os = "linux")]
mod agent;
#[cfg(target_os = "linux")]
#[path = "../../src/guest_protocol.rs"]
mod protocol;

use std::process::ExitCode;

fn main() -> ExitCode {
    #[cfg(target_os = "linux")]
    {
        if let Err(error) = pid1::boot() {
            eprintln!("factory-guest: boot failed: {error}");
        } else if let Err(error) = agent::session() {
            eprintln!("factory-guest: {error}");
        }
        pid1::shutdown()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("factory-guest only runs as a microvm's init on Linux");
        ExitCode::FAILURE
    }
}

/// What only the first process of a Linux machine does.
#[cfg(target_os = "linux")]
mod pid1 {
    use std::{
        ffi::CString,
        fs, io, mem,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::net::UnixStream,
        },
        process::ExitCode,
        ptr,
    };

    /// Shared, read-only root filesystem: the published squashfs, unmodified.
    const ROOT_DEVICE: &str = "/dev/vda";
    /// This session's own disk: the writable layer, workspace and harness state.
    const SESSION_DEVICE: &str = "/dev/vdb";
    /// Shared, read-only tools: mise and the setup script.
    const TOOLS_DEVICE: &str = "/dev/vdc";

    fn cstring(value: &str) -> CString {
        CString::new(value).expect("no interior NUL")
    }

    fn mount(
        source: &str,
        target: &str,
        fstype: Option<&str>,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> io::Result<()> {
        fs::create_dir_all(target)?;
        let (source, target) = (cstring(source), cstring(target));
        let fstype = fstype.map(cstring);
        let data = data.map(cstring);
        // SAFETY: every pointer is a live NUL-terminated string or null.
        let result = unsafe {
            libc::mount(
                source.as_ptr(),
                target.as_ptr(),
                fstype.as_ref().map_or(ptr::null(), |f| f.as_ptr()),
                flags,
                data.as_ref().map_or(ptr::null(), |d| d.as_ptr().cast()),
            )
        };
        if result == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            Err(io::Error::new(
                error.kind(),
                format!("mount {target:?}: {error}"),
            ))
        }
    }

    fn check(result: libc::c_int, what: &str) -> io::Result<()> {
        if result == 0 {
            Ok(())
        } else {
            let error = io::Error::last_os_error();
            Err(io::Error::new(error.kind(), format!("{what}: {error}")))
        }
    }

    /// The initramfs has no console device until devtmpfs is mounted.
    fn attach_console() {
        if let Ok(console) = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/console")
        {
            for fd in 0..=2 {
                // SAFETY: duplicating an open descriptor onto the standard ones.
                unsafe { libc::dup2(console.as_raw_fd(), fd) };
            }
        }
    }

    /// The proxy forwarder listens on loopback, which starts out down.
    fn loopback_up() -> io::Result<()> {
        // SAFETY: a zeroed ifreq is valid, the name fits with its NUL, and the
        // socket is closed on every path.
        unsafe {
            let socket = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            if socket < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut request: libc::ifreq = mem::zeroed();
            for (slot, byte) in request.ifr_name.iter_mut().zip(b"lo") {
                *slot = *byte as libc::c_char;
            }
            let mut result = libc::ioctl(socket, libc::SIOCGIFFLAGS as _, &mut request);
            if result == 0 {
                request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
                result = libc::ioctl(socket, libc::SIOCSIFFLAGS as _, &request);
            }
            let outcome = check(result, "bring up lo");
            libc::close(socket);
            outcome
        }
    }

    pub fn boot() -> io::Result<()> {
        mount("proc", "/proc", Some("proc"), 0, None)?;
        mount("sysfs", "/sys", Some("sysfs"), 0, None)?;
        mount("devtmpfs", "/dev", Some("devtmpfs"), 0, None)?;
        attach_console();

        mount(
            ROOT_DEVICE,
            "/lower",
            Some("squashfs"),
            libc::MS_RDONLY,
            None,
        )?;
        mount(SESSION_DEVICE, "/disk", Some("ext4"), 0, None)?;
        // `data` is what the session sees; the overlay's own directories stay
        // out of its view, because writing to them behind its back is undefined.
        for dir in ["/disk/upper", "/disk/work", "/disk/data"] {
            fs::create_dir_all(dir)?;
        }
        mount(
            "overlay",
            "/newroot",
            Some("overlay"),
            0,
            Some("lowerdir=/lower,upperdir=/disk/upper,workdir=/disk/work"),
        )?;
        mount("/disk/data", "/newroot/session", None, libc::MS_BIND, None)?;
        mount(
            TOOLS_DEVICE,
            "/newroot/opt/factory",
            Some("ext4"),
            libc::MS_RDONLY,
            None,
        )?;
        for dir in ["/proc", "/sys", "/dev"] {
            mount(dir, &format!("/newroot{dir}"), None, libc::MS_MOVE, None)?;
        }

        // Make the overlay the root, the way switch_root does.
        std::env::set_current_dir("/newroot")?;
        mount(".", "/", None, libc::MS_MOVE, None)?;
        let dot = cstring(".");
        // SAFETY: a live NUL-terminated path.
        check(unsafe { libc::chroot(dot.as_ptr()) }, "chroot")?;
        std::env::set_current_dir("/")?;

        mount("tmpfs", "/tmp", Some("tmpfs"), 0, Some("mode=1777"))?;
        mount("tmpfs", "/run", Some("tmpfs"), 0, Some("mode=0755"))?;
        mount("tmpfs", "/dev/shm", Some("tmpfs"), 0, Some("mode=1777"))?;
        // Only terminal emulation needs this; a guest without it still works.
        let _ = mount("devpts", "/dev/pts", Some("devpts"), 0, None);
        loopback_up()
    }

    pub fn vsock_connect(port: u32) -> io::Result<UnixStream> {
        // SAFETY: the address is a fully initialised sockaddr_vm of the size
        // passed, and the descriptor is owned by the returned stream or closed.
        unsafe {
            let fd = libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut address: libc::sockaddr_vm = mem::zeroed();
            address.svm_family = libc::AF_VSOCK as libc::sa_family_t;
            address.svm_cid = libc::VMADDR_CID_HOST;
            address.svm_port = port;
            let size = mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t;
            if libc::connect(fd, ptr::addr_of!(address).cast(), size) != 0 {
                let error = io::Error::last_os_error();
                libc::close(fd);
                return Err(error);
            }
            Ok(UnixStream::from_raw_fd(fd))
        }
    }

    /// Flushes the session disk and resets the machine, which makes
    /// Firecracker exit. The first process must never return.
    pub fn shutdown() -> ExitCode {
        // SAFETY: both calls take no pointers.
        unsafe {
            libc::sync();
            libc::reboot(libc::RB_AUTOBOOT);
        }
        ExitCode::FAILURE
    }
}
