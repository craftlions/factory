//! The session itself: setup, harness and proxy, all over vsock.

use crate::{
    pid1,
    protocol::{
        CONTROL_PORT, GuestEvent, GuestUser, HostMessage, PROXY_PORT, Phase, STDIO_PORT, Spec,
    },
};
use std::{
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    net::{Shutdown, TcpListener},
    os::unix::{
        fs::{MetadataExt, PermissionsExt},
        net::UnixStream,
        process::CommandExt,
    },
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_GRACE: Duration = Duration::from_secs(3);

/// A vsock stream is a plain stream socket, so it is handled as one.
fn connect(port: u32) -> io::Result<UnixStream> {
    pid1::vsock_connect(port)
}

fn connect_with_retry(port: u32) -> io::Result<UnixStream> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        match connect(port) {
            Ok(stream) => return Ok(stream),
            Err(error) if Instant::now() >= deadline => return Err(error),
            Err(_) => thread::sleep(Duration::from_millis(100)),
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
fn serve_proxy(listener: TcpListener) {
    for local in listener.incoming().flatten() {
        thread::spawn(move || {
            let Ok(remote) = connect(PROXY_PORT) else {
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
    command
        .args(args)
        .envs(&spec.env)
        .current_dir(&spec.cwd)
        .uid(spec.user.uid)
        .gid(spec.user.gid);
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
) -> io::Result<Option<i32>> {
    let stdio = connect_with_retry(STDIO_PORT)?;
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

/// Adds `line` to an account database unless an entry already has `id`.
fn add_account(file: &str, id: u32, line: &str) -> io::Result<()> {
    let existing = fs::read_to_string(file).unwrap_or_default();
    let id = id.to_string();
    if existing
        .lines()
        .any(|entry| entry.split(':').nth(2) == Some(id.as_str()))
    {
        return Ok(());
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n")?;
    }
    file.write_all(line.as_bytes())
}

/// Symbolic links are changed themselves and never followed.
fn chown_tree(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if meta.uid() != uid || meta.gid() != gid {
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path)? {
            chown_tree(&entry?.path(), uid, gid)?;
        }
    }
    Ok(())
}

/// Makes the user known to the system, so tools can look it up, and hands it
/// its directories. A disk from before sessions ran unprivileged is root's.
fn adopt(user: &GuestUser, home: &str) -> io::Result<()> {
    let GuestUser { name, uid, gid, .. } = user;
    add_account("/etc/group", *gid, &format!("{name}:x:{gid}:\n"))?;
    add_account(
        "/etc/passwd",
        *uid,
        &format!("{name}:x:{uid}:{gid}::{home}:/bin/sh\n"),
    )?;
    for dir in &user.owns {
        chown_tree(Path::new(dir), *uid, *gid)?;
    }
    Ok(())
}

pub fn session() -> io::Result<()> {
    let stream = connect_with_retry(CONTROL_PORT)?;
    let mut control_reader = BufReader::new(stream.try_clone()?);
    let control: Control = Arc::new(Mutex::new(stream));

    let outcome = (|| -> io::Result<()> {
        let mut line = String::new();
        control_reader.read_line(&mut line)?;
        let spec: Spec = serde_json::from_str(&line)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        write_files(&spec)?;
        adopt(&spec.user, spec.env.get("HOME").map_or("/", String::as_str))?;

        let listener = TcpListener::bind(("127.0.0.1", spec.proxy_port))?;
        thread::spawn(move || serve_proxy(listener));

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
        let code = run_harness(&spec, &control, control_reader)?;
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
