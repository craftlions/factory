//! The guest agent: the first process of a session's microvm.
//!
//! It is started from the initramfs. It layers the session's disk over the
//! shared read-only root filesystem, connects to the factory over vsock, runs
//! the setup command and then the harness with the harness's stdio bridged to
//! the factory, and forwards the guest's outbound TCP connections to the
//! factory's proxy, which is the guest's only way out. When the harness ends
//! it shuts the machine down.
//!
//! `--test-sockets <dir>` runs the same logic as an ordinary process that
//! talks to unix sockets in `<dir>`, named the way Firecracker names them. The
//! factory's tests use it in place of a real microvm.

#[path = "../../src/guest_protocol.rs"]
mod protocol;

use protocol::{CONTROL_PORT, GuestEvent, HostMessage, PROXY_PORT, Phase, STDIO_PORT, Spec};
use std::{
    env, fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_GRACE: Duration = Duration::from_secs(3);

#[derive(Clone)]
enum Transport {
    #[cfg(target_os = "linux")]
    Vsock,
    Unix(PathBuf),
}

impl Transport {
    /// A vsock stream is a plain stream socket, so it is handled as one.
    fn connect(&self, port: u32) -> io::Result<UnixStream> {
        match self {
            #[cfg(target_os = "linux")]
            Transport::Vsock => pid1::vsock_connect(port),
            Transport::Unix(dir) => UnixStream::connect(dir.join(format!("v.sock_{port}"))),
        }
    }

    fn connect_with_retry(&self, port: u32) -> io::Result<UnixStream> {
        let deadline = Instant::now() + CONNECT_TIMEOUT;
        loop {
            match self.connect(port) {
                Ok(stream) => return Ok(stream),
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

type Control = Arc<Mutex<UnixStream>>;

fn send(control: &Control, event: &GuestEvent) {
    let mut line = serde_json::to_string(event).expect("events serialise");
    line.push('\n');
    // A lost control channel ends the session from the host's side anyway.
    let _ = control
        .lock()
        .expect("control lock")
        .write_all(line.as_bytes());
}

fn copy_then_close(mut from: impl Read, mut to: impl Write, close: impl FnOnce()) {
    let _ = io::copy(&mut from, &mut to);
    close();
}

/// Every local connection becomes one vsock connection to the factory's proxy.
fn serve_proxy(listener: TcpListener, transport: Transport) {
    for local in listener.incoming().flatten() {
        let transport = transport.clone();
        thread::spawn(move || {
            let Ok(remote) = transport.connect(PROXY_PORT) else {
                return;
            };
            let (Ok(local_read), Ok(remote_read)) = (local.try_clone(), remote.try_clone()) else {
                return;
            };
            let upstream = thread::spawn(move || {
                copy_then_close(local_read, &remote, || {
                    let _ = remote.shutdown(Shutdown::Write);
                });
            });
            copy_then_close(remote_read, &local, || {
                let _ = local.shutdown(Shutdown::Write);
            });
            let _ = upstream.join();
        });
    }
}

fn forward_lines(
    reader: impl Read + Send + 'static,
    control: Control,
    phase: Phase,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            send(&control, &GuestEvent::Log { phase, line });
        }
    })
}

fn command(argv: &[String], spec: &Spec) -> io::Result<Command> {
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty command"))?;
    let mut command = Command::new(program);
    command.args(args).envs(&spec.env).current_dir(&spec.cwd);
    Ok(command)
}

fn run_setup(spec: &Spec, control: &Control) -> io::Result<Option<i32>> {
    let mut child = command(&spec.setup, spec)?
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let out = forward_lines(
        child.stdout.take().expect("piped"),
        control.clone(),
        Phase::Setup,
    );
    let err = forward_lines(
        child.stderr.take().expect("piped"),
        control.clone(),
        Phase::Setup,
    );
    let status = child.wait()?;
    let _ = (out.join(), err.join());
    Ok(status.code())
}

fn terminate(pid: u32) {
    // SAFETY: plain signal delivery to a process id we spawned.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    thread::sleep(STOP_GRACE);
    // Harmless if it already left: the agent shuts the machine down right after.
    unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
}

fn run_harness(
    spec: &Spec,
    control: &Control,
    control_reader: BufReader<UnixStream>,
    transport: &Transport,
) -> io::Result<Option<i32>> {
    let stdio = transport.connect_with_retry(STDIO_PORT)?;
    let mut child = command(&spec.harness, spec)?
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id();
    let stdin = child.stdin.take().expect("piped");
    let stdout = child.stdout.take().expect("piped");
    let err = forward_lines(
        child.stderr.take().expect("piped"),
        control.clone(),
        Phase::Harness,
    );

    // Host to harness. EOF from the host closes the harness's stdin.
    let from_host = stdio.try_clone()?;
    thread::spawn(move || copy_then_close(from_host, stdin, || {}));
    let to_host = stdio.try_clone()?;
    let out = thread::spawn(move || {
        copy_then_close(stdout, &to_host, || {
            let _ = to_host.shutdown(Shutdown::Write);
        });
    });

    // A stop request, or the host going away, ends the harness.
    thread::spawn(move || {
        for line in control_reader.lines() {
            match line.map(|line| serde_json::from_str::<HostMessage>(&line)) {
                Ok(Ok(HostMessage::Stop)) | Err(_) => break,
                Ok(Err(_)) => continue,
            }
        }
        terminate(pid);
    });

    let status = child.wait()?;
    let _ = (out.join(), err.join());
    Ok(status.code())
}

fn write_files(spec: &Spec) -> io::Result<()> {
    fs::create_dir_all(&spec.cwd)?;
    // Tools expect their home to exist.
    if let Some(home) = spec.env.get("HOME") {
        fs::create_dir_all(home)?;
    }
    for file in &spec.files {
        let path = Path::new(&file.path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(path, &file.content)?;
        fs::set_permissions(path, fs::Permissions::from_mode(file.mode))?;
    }
    Ok(())
}

fn session(transport: Transport) -> io::Result<()> {
    let stream = transport.connect_with_retry(CONTROL_PORT)?;
    let mut control_reader = BufReader::new(stream.try_clone()?);
    let control: Control = Arc::new(Mutex::new(stream));

    let outcome = (|| -> io::Result<()> {
        let mut line = String::new();
        control_reader.read_line(&mut line)?;
        let spec: Spec = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_files(&spec)?;

        let listener = TcpListener::bind(("127.0.0.1", spec.proxy_port))?;
        let proxy_transport = transport.clone();
        thread::spawn(move || serve_proxy(listener, proxy_transport));

        if !spec.setup.is_empty() {
            send(
                &control,
                &GuestEvent::Phase {
                    phase: Phase::Setup,
                },
            );
            let code = run_setup(&spec, &control)?;
            send(
                &control,
                &GuestEvent::Exit {
                    phase: Phase::Setup,
                    code,
                },
            );
            if code != Some(0) {
                return Ok(());
            }
        }

        send(
            &control,
            &GuestEvent::Phase {
                phase: Phase::Harness,
            },
        );
        let code = run_harness(&spec, &control, control_reader, &transport)?;
        send(
            &control,
            &GuestEvent::Exit {
                phase: Phase::Harness,
                code,
            },
        );
        Ok(())
    })();

    if let Err(error) = &outcome {
        send(
            &control,
            &GuestEvent::Error {
                message: error.to_string(),
            },
        );
    }
    outcome
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    if let Some(dir) = args
        .iter()
        .position(|arg| arg == "--test-sockets")
        .and_then(|i| args.get(i + 1))
    {
        return match session(Transport::Unix(PathBuf::from(dir))) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("factory-guest: {error}");
                ExitCode::FAILURE
            }
        };
    }

    #[cfg(target_os = "linux")]
    {
        if let Err(error) = pid1::boot() {
            eprintln!("factory-guest: boot failed: {error}");
        } else if let Err(error) = session(Transport::Vsock) {
            eprintln!("factory-guest: {error}");
        }
        pid1::shutdown()
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!("factory-guest only runs as a microvm's init on Linux, or with --test-sockets");
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
