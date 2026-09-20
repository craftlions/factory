//! Runs a harness inside a Firecracker microvm.
//!
//! Each session has its own disk. The guest boots the shared read-only root
//! filesystem with that disk layered on top, so whatever the session installs
//! or writes stays on its disk. The guest has no network device. Its agent
//! reaches the factory over vsock, which carries the harness's stdio and a
//! proxy that only connects to an allow-list of hosts.

pub mod assets;
pub mod proxy;

use crate::{
    guest_protocol::{
        CONTROL_PORT, GuestEvent, GuestFile, HostMessage, PROXY_PORT, Phase, STDIO_PORT, Spec,
    },
    harness::{Invocation, Process},
};
use proxy::AllowList;
use std::{
    collections::BTreeMap,
    env, io,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    process::{Child, Command},
    sync::{mpsc, oneshot},
    time::timeout,
};

const BOOT_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);
const SESSION_DISK_SIZE: u64 = 16 * 1024 * 1024 * 1024;
const LOG_TAIL: usize = 20;
/// Hosts mise needs to install the tools in `packaging/guest/mise.toml` from
/// GitHub releases. Found by running the setup script in the published root
/// filesystem with nothing allowed and reading what the proxy refused.
const TOOL_HOSTS: [&str; 3] = [
    "api.github.com",
    "github.com",
    "release-assets.githubusercontent.com",
];

/// One start of a guest.
pub struct VmRequest<'a> {
    /// The session's directory on the host; holds its disk and the vm's files.
    pub dir: &'a Path,
    pub invocation: &'a Invocation,
    /// Written inside the guest before anything runs.
    pub files: Vec<GuestFile>,
    /// Hosts the harness may reach, beyond what tool setup needs.
    pub hosts: Vec<String>,
    /// Setup output, and everything the proxy refuses, as readable lines.
    pub notices: mpsc::UnboundedSender<String>,
}

#[derive(Clone, Debug)]
pub struct Microvm {
    pub firecracker: PathBuf,
    pub initrd: PathBuf,
    /// Holds `startup.sh` and `mise.toml` for the tools drive.
    pub guest_dir: PathBuf,
    /// Shared kernel, root filesystem and tools drive.
    pub assets_dir: PathBuf,
    /// Where the session's data lives inside the guest.
    pub guest_root: String,
    /// Runs before the harness. Empty skips setup.
    pub setup: Vec<String>,
    /// Put in front of the harness command line, so it runs with its tools.
    pub wrapper: Vec<String>,
    /// Loopback port inside the guest that leads to the factory's proxy.
    pub guest_proxy_port: u16,
    pub vcpus: u8,
    pub memory_mib: u32,
}

impl Microvm {
    pub fn from_env(data_dir: &Path) -> Self {
        let lib = PathBuf::from("/usr/lib/craftlions-factory");
        let path = |key: &str, default: PathBuf| env::var_os(key).map_or(default, PathBuf::from);
        Self {
            firecracker: path("FACTORY_FIRECRACKER_BIN", lib.join("firecracker")),
            initrd: path("FACTORY_GUEST_INITRD", lib.join("guest-initramfs.cpio")),
            guest_dir: path("FACTORY_GUEST_DIR", lib.join("guest")),
            assets_dir: data_dir.join("microvm"),
            guest_root: "/session".into(),
            setup: vec!["/bin/sh".into(), "/opt/factory/startup.sh".into()],
            wrapper: vec!["/opt/factory/bin/mise".into(), "exec".into(), "--".into()],
            guest_proxy_port: 3128,
            vcpus: 2,
            memory_mib: 2048,
        }
    }

    pub fn workspace(&self) -> String {
        format!("{}/workspace", self.guest_root)
    }

    /// Where the harness keeps its own session file, inside the guest.
    pub fn state_dir(&self) -> String {
        format!("{}/harness", self.guest_root)
    }

    pub fn home(&self) -> String {
        format!("{}/home", self.guest_root)
    }

    fn spec(&self, request: &VmRequest) -> Spec {
        let home = self.home();
        let proxy = format!("http://127.0.0.1:{}", self.guest_proxy_port);
        let env: BTreeMap<String, String> = [
            ("HOME", home.clone()),
            ("PATH", format!("{home}/.local/share/mise/shims:/opt/factory/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")),
            ("LANG", "C.UTF-8".into()),
            ("TERM", "dumb".into()),
            // The guest has no network device; the agent forwards this port to
            // the factory's proxy.
            ("HTTPS_PROXY", proxy.clone()),
            ("HTTP_PROXY", proxy.clone()),
            ("https_proxy", proxy.clone()),
            ("http_proxy", proxy),
            ("NO_PROXY", "127.0.0.1,localhost".into()),
            ("NODE_USE_ENV_PROXY", "1".into()),
            ("MISE_GLOBAL_CONFIG_FILE", "/opt/factory/mise.toml".into()),
            ("MISE_TRUSTED_CONFIG_PATHS", "/opt/factory".into()),
            ("MISE_YES", "1".into()),
            // Its startup network calls would only be refused by the proxy.
            ("PI_OFFLINE", "1".into()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();

        let mut harness = self.wrapper.clone();
        harness.push(request.invocation.program.clone());
        harness.extend(request.invocation.args.iter().cloned());
        Spec {
            files: request
                .files
                .iter()
                .map(|f| GuestFile {
                    path: f.path.clone(),
                    mode: f.mode,
                    content: f.content.clone(),
                })
                .collect(),
            env,
            setup: self.setup.clone(),
            harness,
            cwd: self.workspace(),
            proxy_port: self.guest_proxy_port,
        }
    }

    fn config(&self, assets: &assets::Assets, disk: &Path) -> serde_json::Value {
        let drive = |id: &str, path: &Path, read_only: bool| {
            serde_json::json!({
                "drive_id": id, "path_on_host": path, "is_root_device": false, "is_read_only": read_only,
            })
        };
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": assets.kernel,
                "initrd_path": self.initrd,
                // The agent in the initramfs mounts the drives itself.
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off quiet loglevel=3",
            },
            // The order fixes the device names the agent expects: vda, vdb, vdc.
            "drives": [
                drive("rootfs", &assets.rootfs, true),
                drive("session", disk, false),
                drive("tools", &assets.tools, true),
            ],
            "machine-config": { "vcpu_count": self.vcpus, "mem_size_mib": self.memory_mib },
            "vsock": { "guest_cid": 3, "uds_path": "v.sock" },
        })
    }

    /// Boots a guest, runs its setup, and returns the harness's pipes. Dropping
    /// the returned future, for instance on a stop during setup, kills the vm.
    pub async fn start(&self, request: VmRequest<'_>) -> io::Result<Process> {
        let assets = assets::ensure(&self.assets_dir, &self.guest_dir, &request.notices).await?;
        let vm_dir = request.dir.join("vm");
        let _ = fs::remove_dir_all(&vm_dir).await;
        fs::create_dir_all(&vm_dir).await?;
        let disk = request.dir.join("disk.ext4");
        if !disk.exists() {
            let file = fs::File::create(&disk).await?;
            // Sparse: it only takes the space the session actually uses.
            file.set_len(SESSION_DISK_SIZE).await?;
            drop(file);
            assets::run("mkfs.ext4", &["-q", "-F", &disk.to_string_lossy()]).await?;
        }

        // The guest connects as soon as it is up, so listen before it boots.
        let listen = |port: u32| UnixListener::bind(vm_dir.join(format!("v.sock_{port}")));
        let (control_listener, stdio_listener, proxy_listener) = (
            listen(CONTROL_PORT)?,
            listen(STDIO_PORT)?,
            listen(PROXY_PORT)?,
        );

        fs::write(
            vm_dir.join("config.json"),
            self.config(&assets, &disk).to_string(),
        )
        .await?;
        let console = std::fs::File::create(vm_dir.join("console.log"))?;
        let mut command = Command::new(&self.firecracker);
        command
            .args(["--no-api", "--config-file", "config.json"])
            .current_dir(&vm_dir)
            .stdin(Stdio::null())
            .stdout(console.try_clone()?)
            .stderr(console)
            .kill_on_drop(true);
        die_with_parent(&mut command);
        let mut child = command.spawn().map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("starting {}: {error}", self.firecracker.display()),
            )
        })?;

        let mut hosts: Vec<String> = TOOL_HOSTS.iter().map(|host| (*host).to_owned()).collect();
        hosts.extend(request.hosts.iter().cloned());
        let allow = Arc::new(AllowList::new(hosts));
        let proxy_notices = request.notices.clone();
        let proxy_task = tokio::spawn(async move {
            while let Ok((stream, _)) = proxy_listener.accept().await {
                tokio::spawn(proxy::serve(stream, allow.clone(), proxy_notices.clone()));
            }
        });
        let proxy_guard = AbortOnDrop(proxy_task);

        let console_tail = || tail_of(&vm_dir.join("console.log"));
        let control = tokio::select! {
            accepted = timeout(BOOT_TIMEOUT, control_listener.accept()) => match accepted {
                Ok(Ok((stream, _))) => stream,
                Ok(Err(error)) => return Err(error),
                Err(_) => return Err(io::Error::other(format!("the guest did not come up. {}", console_tail()))),
            },
            status = child.wait() => {
                return Err(io::Error::other(format!("firecracker exited early ({status:?}). {}", console_tail())));
            }
        };
        let (control_read, mut control_write) = control.into_split();
        let mut spec = serde_json::to_string(&self.spec(&request)).map_err(io::Error::other)?;
        spec.push('\n');
        control_write.write_all(spec.as_bytes()).await?;

        let mut events = BufReader::new(control_read).lines();
        loop {
            let Some(line) = events.next_line().await? else {
                return Err(io::Error::other(format!(
                    "the guest went away during setup. {}",
                    console_tail()
                )));
            };
            match serde_json::from_str::<GuestEvent>(&line) {
                Ok(GuestEvent::Log { line, .. }) => {
                    let _ = request.notices.send(line);
                }
                Ok(GuestEvent::Exit {
                    phase: Phase::Setup,
                    code,
                }) if code != Some(0) => {
                    return Err(io::Error::other(format!(
                        "tool setup failed with exit code {code:?}"
                    )));
                }
                Ok(GuestEvent::Error { message }) => {
                    return Err(io::Error::other(format!("guest agent: {message}")));
                }
                Ok(GuestEvent::Phase {
                    phase: Phase::Harness,
                }) => break,
                _ => {}
            }
        }
        let (stdio, _) = timeout(BOOT_TIMEOUT, stdio_listener.accept())
            .await
            .map_err(|_| io::Error::other("the guest did not connect the harness"))??;
        let (stdout, stdin) = stdio.into_split();

        let (stop, stop_rx) = oneshot::channel::<()>();
        let (exited_tx, exited) = oneshot::channel();
        tokio::spawn(async move {
            let failure = supervise(&mut child, events, control_write, stop_rx).await;
            drop(proxy_guard);
            let _ = exited_tx.send(failure);
        });
        Ok(Process {
            stdin: Box::new(stdin),
            stdout: Box::new(stdout),
            stop,
            exited,
        })
    }

    /// Copies the harness state and the workspace out of an ended session's
    /// disk, into `harness/` and `workspace/` next to it.
    pub async fn extract(&self, dir: &Path) -> io::Result<()> {
        let disk = dir.join("disk.ext4");
        if !disk.exists() {
            return Ok(());
        }
        // Skipped when the disk has not changed since the last extraction.
        let stamp = dir.join("extracted.stamp");
        let modified = |path: &Path| {
            std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
        };
        if let (Some(disk_time), Some(stamp_time)) = (modified(&disk), modified(&stamp))
            && stamp_time >= disk_time
        {
            return Ok(());
        }
        let disk_str = disk.to_string_lossy().into_owned();
        // A vm that was killed leaves the journal unreplayed; 0 and 1 are fine.
        let _ = assets::run("e2fsck", &["-p", &disk_str]).await;
        let staging = dir.join("extract");
        let _ = fs::remove_dir_all(&staging).await;
        fs::create_dir_all(&staging).await?;
        let staging_str = staging.to_string_lossy().into_owned();
        for name in ["harness", "workspace"] {
            let request = format!("rdump /data/{name} {staging_str}");
            assets::run("debugfs", &["-R", &request, &disk_str]).await?;
            if staging.join(name).is_dir() {
                let target = dir.join(name);
                let _ = fs::remove_dir_all(&target).await;
                fs::rename(staging.join(name), target).await?;
            }
        }
        fs::remove_dir_all(&staging).await?;
        fs::write(&stamp, "").await
    }
}

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn tail_of(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().rev().take(LOG_TAIL).collect();
    let tail: Vec<&str> = lines.into_iter().rev().collect();
    if tail.is_empty() {
        "The console is empty.".into()
    } else {
        format!("Console: {}", tail.join(" | "))
    }
}

/// A vm must not outlive the factory, even if the factory is killed outright.
fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    // SAFETY: prctl is async-signal-safe and touches no memory of ours.
    unsafe {
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) == 0 {
                Ok(())
            } else {
                Err(io::Error::last_os_error())
            }
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Follows a running guest to its end and says why it ended, if it failed.
async fn supervise(
    child: &mut Child,
    mut events: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    mut control: tokio::net::unix::OwnedWriteHalf,
    stop: oneshot::Receiver<()>,
) -> Option<String> {
    let mut stderr_tail: Vec<String> = Vec::new();
    let mut exit_code: Option<Option<i32>> = None;
    let mut stopped = false;
    let mut stop = Box::pin(async move {
        if stop.await.is_err() {
            std::future::pending::<()>().await;
        }
    });
    // Armed by a stop, so a guest that ignores it cannot hold the session open.
    let mut deadline: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
        Box::pin(std::future::pending());

    loop {
        tokio::select! {
            () = &mut stop, if !stopped => {
                stopped = true;
                let mut message = serde_json::to_string(&HostMessage::Stop).expect("serialises");
                message.push('\n');
                let _ = control.write_all(message.as_bytes()).await;
                deadline = Box::pin(tokio::time::sleep(SHUTDOWN_TIMEOUT));
            }
            () = &mut deadline => break,
            line = events.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<GuestEvent>(&line) {
                    Ok(GuestEvent::Log { line, .. }) => {
                        if stderr_tail.len() == LOG_TAIL {
                            stderr_tail.remove(0);
                        }
                        stderr_tail.push(line);
                    }
                    Ok(GuestEvent::Exit { phase: Phase::Harness, code }) => exit_code = Some(code),
                    Ok(GuestEvent::Error { message }) => stderr_tail.push(message),
                    _ => {}
                },
                // The guest is shutting down, or gone.
                _ => break,
            },
        }
    }
    if timeout(SHUTDOWN_TIMEOUT, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }

    match exit_code {
        _ if stopped => None,
        Some(Some(0)) => None,
        Some(code) => Some(
            format!(
                "the harness exited with code {code:?}. {}",
                stderr_tail.join(" | ")
            )
            .trim()
            .to_owned(),
        ),
        None => Some(
            format!("the guest went away. {}", stderr_tail.join(" | "))
                .trim()
                .to_owned(),
        ),
    }
}

/// A path the host can open for a file the harness named inside the guest.
pub fn extracted(state_dir: &Path, guest_file: &str) -> Option<PathBuf> {
    Path::new(guest_file)
        .file_name()
        .map(|name| state_dir.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncReadExt;

    /// Builds the guest agent and a stand-in for Firecracker that runs it as a
    /// plain process. It talks to the same unix sockets a real vm's vsock would.
    fn test_vm(root: &Path) -> Microvm {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR"));
        let built = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "factory-guest"])
            .current_dir(repo)
            .status()
            .unwrap();
        assert!(built.success());
        let guest = repo.join("target/debug/factory-guest");

        let _ = std::fs::remove_dir_all(root);
        let assets_dir = root.join("assets");
        std::fs::create_dir_all(&assets_dir).unwrap();
        let firecracker = root.join("fake-firecracker");
        std::fs::write(
            &firecracker,
            format!(
                "#!/bin/sh\nexec {} --test-sockets \"$PWD\"\n",
                guest.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&firecracker, std::fs::Permissions::from_mode(0o755)).unwrap();
        // With every asset in place nothing is downloaded or built.
        let guest_dir = repo.join("packaging/guest");
        assets::prefill_for_tests(&assets_dir, &guest_dir);

        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        Microvm {
            firecracker,
            initrd: root.join("unused"),
            guest_dir,
            assets_dir,
            guest_root: root.join("guest").to_string_lossy().into_owned(),
            setup: vec![
                "/bin/sh".into(),
                "-c".into(),
                "echo installing tools".into(),
            ],
            wrapper: Vec::new(),
            guest_proxy_port: port,
            vcpus: 1,
            memory_mib: 128,
        }
    }

    fn session_dir(root: &Path) -> PathBuf {
        let dir = root.join("s");
        std::fs::create_dir_all(&dir).unwrap();
        // Present already, so no filesystem tools are needed.
        std::fs::write(dir.join("disk.ext4"), "").unwrap();
        dir
    }

    // Unix socket paths are short, so the test lives directly under /tmp.
    fn root(name: &str) -> PathBuf {
        PathBuf::from(format!("/tmp/fvm-{name}-{}", std::process::id()))
    }

    #[tokio::test]
    async fn a_guest_runs_setup_then_the_harness_behind_the_proxy() {
        let root = root("echo");
        let vm = test_vm(&root);
        let dir = session_dir(&root);
        let invocation = Invocation {
            program: "/bin/sh".into(),
            args: vec![
                "-c".into(),
                "cat \"$HOME/greeting\"; while read line; do echo \"got $line\"; done".into(),
            ],
        };
        let (notices, mut seen) = mpsc::unbounded_channel();
        let files = vec![GuestFile {
            path: format!("{}/greeting", vm.home()),
            mode: 0o600,
            content: "hello from the host\n".into(),
        }];
        let request = VmRequest {
            dir: &dir,
            invocation: &invocation,
            files,
            hosts: vec!["openrouter.ai".into()],
            notices,
        };
        let Process {
            mut stdin,
            stdout,
            stop,
            exited,
        } = vm.start(request).await.unwrap();
        assert_eq!(seen.recv().await.unwrap(), "installing tools");

        let mut lines = BufReader::new(stdout).lines();
        assert_eq!(
            lines.next_line().await.unwrap().unwrap(),
            "hello from the host"
        );
        stdin.write_all(b"ping\n").await.unwrap();
        assert_eq!(lines.next_line().await.unwrap().unwrap(), "got ping");

        // The guest's way out refuses hosts that are not on the list.
        let mut out = tokio::net::TcpStream::connect(("127.0.0.1", vm.guest_proxy_port))
            .await
            .unwrap();
        out.write_all(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        out.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(seen.recv().await.unwrap().contains("example.com:443"));

        stop.send(()).unwrap();
        assert_eq!(exited.await.unwrap(), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// Needs pi installed with OpenRouter credentials, and sends one tiny prompt.
    /// Proves that pi works with the proxy as its only way out, and which hosts
    /// it needs. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn pi_reaches_its_model_through_the_proxy_only() {
        use crate::harness::{
            ChatEvent, ChatItem, Command as HarnessCommand, Harness, LaunchSpec, Resume, Update,
            pi::Pi,
        };
        let root = root("pi");
        let mut vm = test_vm(&root);
        vm.setup = Vec::new();
        let dir = session_dir(&root);
        let state_dir = vm.state_dir();
        let spec = LaunchSpec {
            state_dir: &state_dir,
            provider: "openrouter",
            model: "z-ai/glm-5.3-flash",
            reasoning: "low",
            resume: Resume::No,
        };
        let mut invocation = Pi.invocation(&spec);
        // The guest environment's PATH has no host tools on it.
        let which = std::process::Command::new("which")
            .arg("pi")
            .output()
            .unwrap();
        invocation.program = String::from_utf8(which.stdout).unwrap().trim().to_owned();
        let auth = std::fs::read_to_string(
            Path::new(&env::var("HOME").unwrap()).join(".pi/agent/auth.json"),
        )
        .unwrap();
        let files = vec![GuestFile {
            path: format!("{}/.pi/agent/auth.json", vm.home()),
            mode: 0o600,
            content: auth,
        }];
        let (notices, mut seen) = mpsc::unbounded_channel();
        // Set to see pi refused: it must not have another way out.
        let hosts = if env::var_os("FACTORY_TEST_ALLOW_NOTHING").is_some() {
            Vec::new()
        } else {
            vec!["openrouter.ai".to_owned()]
        };
        let request = VmRequest {
            dir: &dir,
            invocation: &invocation,
            files,
            hosts,
            notices,
        };
        let process = vm.start(request).await.unwrap();

        let (commands, command_rx) = mpsc::unbounded_channel();
        let (update_tx, mut updates) = mpsc::unbounded_channel();
        Pi.attach(process, &spec, command_rx, update_tx);
        commands
            .send(HarnessCommand::Prompt(
                "Reply with the single word: pong".into(),
            ))
            .unwrap();
        let mut answer = None;
        while let Some(update) = updates.recv().await {
            match update {
                Update::Event(ChatEvent::MessageEnd {
                    item: ChatItem::Assistant { blocks, error, .. },
                }) => {
                    answer = Some(format!("{blocks:?} {error:?}"));
                }
                Update::Event(ChatEvent::Working { working: false }) => {
                    commands.send(HarnessCommand::Stop).unwrap();
                }
                Update::Exited { failure } => {
                    println!("exited: {failure:?}");
                    break;
                }
                _ => {}
            }
        }
        let mut blocked = Vec::new();
        while let Ok(notice) = seen.try_recv() {
            blocked.push(notice);
        }
        println!("answer: {answer:?}\nnotices: {blocked:#?}");
        assert!(answer.unwrap().to_lowercase().contains("pong"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn a_failing_harness_is_reported_with_its_stderr() {
        let root = root("fail");
        let vm = test_vm(&root);
        let dir = session_dir(&root);
        let invocation = Invocation {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "echo broken >&2; exit 7".into()],
        };
        let (notices, _seen) = mpsc::unbounded_channel();
        let request = VmRequest {
            dir: &dir,
            invocation: &invocation,
            files: Vec::new(),
            hosts: Vec::new(),
            notices,
        };
        let process = vm.start(request).await.unwrap();
        let failure = process.exited.await.unwrap().expect("a failure");
        assert!(
            failure.contains('7') && failure.contains("broken"),
            "{failure}"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    async fn failed_setup_stops_the_start() {
        let root = root("setup");
        let mut vm = test_vm(&root);
        vm.setup = vec![
            "/bin/sh".into(),
            "-c".into(),
            "echo no network; exit 3".into(),
        ];
        let dir = session_dir(&root);
        let invocation = Invocation {
            program: "/bin/true".into(),
            args: Vec::new(),
        };
        let (notices, mut seen) = mpsc::unbounded_channel();
        let request = VmRequest {
            dir: &dir,
            invocation: &invocation,
            files: Vec::new(),
            hosts: Vec::new(),
            notices,
        };
        let error = vm
            .start(request)
            .await
            .err()
            .expect("setup fails")
            .to_string();
        assert!(error.contains('3'), "{error}");
        assert_eq!(seen.recv().await.unwrap(), "no network");
        std::fs::remove_dir_all(&root).unwrap();
    }
}
