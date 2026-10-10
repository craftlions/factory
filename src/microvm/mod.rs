//! Runs a harness inside a Firecracker microvm.
//!
//! Each session has its own disk. The guest boots the shared read-only root
//! filesystem with that disk layered on top, so whatever the session installs
//! or writes stays on its disk. The guest has no network device. Its agent
//! reaches the factory over vsock, which carries the harness's stdio and a
//! proxy that connects to an allow-list of hosts, and to the public internet
//! when the session allows it.

pub mod assets;
pub mod proxy;

use crate::{
    guest_protocol::{
        CONTROL_PORT, GuestEvent, GuestFile, HostMessage, PROXY_PORT, Phase, STDIO_PORT, Spec,
    },
    harness::{Invocation, Process},
};
use proxy::{AllowList, Egress, Policy};
use std::{
    collections::BTreeMap,
    env, io,
    path::{Component, Path, PathBuf},
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
/// Where the tools drive is mounted inside the guest.
const TOOLS_MOUNT: &str = "/opt/factory";

/// Hosts mise needs to install the tools in `packaging/guest/tools/<tools>.toml`.
/// Found by running the setup script in the published root filesystem with
/// nothing allowed and reading what the proxy refused.
fn tool_hosts(tools: &str) -> &'static [&'static str] {
    match tools {
        "pi" => &[
            "api.github.com",
            "github.com",
            "release-assets.githubusercontent.com",
        ],
        // Node from its own downloads, the ACP adapter and the Claude Code
        // binary it depends on from npm.
        "claude" => &["nodejs.org", "registry.npmjs.org"],
        _ => &[],
    }
}

/// One start of a guest.
pub struct VmRequest<'a> {
    /// The session's directory on the host; holds its disk and the vm's files.
    pub dir: &'a Path,
    pub invocation: &'a Invocation,
    /// Names the mise configuration on the tools drive that setup installs.
    pub tools: &'a str,
    /// Written inside the guest before anything runs.
    pub files: Vec<GuestFile>,
    /// Secrets for the harness, passed as environment variables.
    pub env: Vec<(String, String)>,
    /// Hosts the harness may reach, beyond what tool setup needs.
    pub hosts: Vec<String>,
    /// Where else the session may go.
    pub egress: Egress,
    /// Setup output, and everything the proxy refuses, as readable lines.
    pub notices: mpsc::UnboundedSender<String>,
}

#[derive(Clone, Debug)]
pub struct Microvm {
    pub firecracker: PathBuf,
    pub initrd: PathBuf,
    /// Holds `startup.sh` and `tools/<harness>.toml` for the tools drive.
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
            setup: vec!["/bin/sh".into(), format!("{TOOLS_MOUNT}/startup.sh")],
            wrapper: vec![
                format!("{TOOLS_MOUNT}/bin/mise"),
                "exec".into(),
                "--".into(),
            ],
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
        let ca_bundle = format!("{TOOLS_MOUNT}/{}", assets::CA_BUNDLE);
        let mut env: BTreeMap<String, String> = [
            ("HOME", home.clone()),
            ("PATH", format!("{home}/.local/share/mise/shims:{TOOLS_MOUNT}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")),
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
            // OpenSSL, and with it curl, git and Python, and mise read the
            // bundle from here; Node adds it to its own.
            ("SSL_CERT_FILE", ca_bundle.clone()),
            ("NODE_EXTRA_CA_CERTS", ca_bundle),
            (
                "MISE_GLOBAL_CONFIG_FILE",
                format!("{TOOLS_MOUNT}/tools/{}.toml", request.tools),
            ),
            ("MISE_TRUSTED_CONFIG_PATHS", TOOLS_MOUNT.into()),
            ("MISE_YES", "1".into()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
        env.extend(request.invocation.env.iter().cloned());
        env.extend(request.env.iter().cloned());

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
        let log = GuestLog::create(&vm_dir.join("harness.log"));
        log.write(
            "factory",
            &format!(
                "starting {}",
                [request.invocation.program.as_str()]
                    .into_iter()
                    .chain(request.invocation.args.iter().map(String::as_str))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        );
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

        let mut hosts: Vec<String> = tool_hosts(request.tools)
            .iter()
            .map(|host| (*host).to_owned())
            .collect();
        hosts.extend(request.hosts.iter().cloned());
        let policy = Arc::new(Policy {
            allow: AllowList::new(hosts),
            egress: request.egress.clone(),
        });
        // Refusals go to the chat and into the log.
        let (proxy_notices, mut refused) = mpsc::unbounded_channel::<String>();
        let (chat, proxy_log) = (request.notices.clone(), log.clone());
        tokio::spawn(async move {
            while let Some(notice) = refused.recv().await {
                proxy_log.write("proxy", &notice);
                let _ = chat.send(notice);
            }
        });
        let proxy_task = tokio::spawn(async move {
            while let Ok((stream, _)) = proxy_listener.accept().await {
                tokio::spawn(proxy::serve(stream, policy.clone(), proxy_notices.clone()));
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
            let event = serde_json::from_str::<GuestEvent>(&line);
            if let Ok(event) = &event {
                log.event(event);
            }
            match event {
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
            let failure = supervise(&mut child, events, control_write, stop_rx, &log).await;
            log.write(
                "factory",
                &format!(
                    "the vm ended: {}",
                    failure.as_deref().unwrap_or("without a failure")
                ),
            );
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

    /// A path the host can open for a file the harness named inside the guest,
    /// once `extract` has copied the harness state to `state_dir`. A file from
    /// outside the guest's state directory is looked for by its name.
    pub fn extracted(&self, state_dir: &Path, guest_file: &str) -> Option<PathBuf> {
        let guest_file = Path::new(guest_file);
        match guest_file.strip_prefix(self.state_dir()) {
            Ok(relative)
                if relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_))) =>
            {
                Some(state_dir.join(relative))
            }
            _ => guest_file.file_name().map(|name| state_dir.join(name)),
        }
    }
}

/// `harness.log` in a vm's directory, written as things happen: what setup
/// printed, the harness's stderr, how each phase ended, and what the proxy
/// refused. Losing it never stops a session.
#[derive(Clone)]
struct GuestLog(Arc<std::sync::Mutex<Option<std::fs::File>>>);

/// A UTC time such as `2026-10-10T17:13:42Z`, after Howard Hinnant's
/// civil-from-days algorithm.
fn utc(time: std::time::SystemTime) -> String {
    let secs = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs() as i64);
    let (days, clock) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        clock / 3600,
        clock % 3600 / 60,
        clock % 60
    )
}

impl GuestLog {
    fn create(path: &Path) -> Self {
        let file = std::fs::File::create(path)
            .inspect_err(|error| eprintln!("cannot write {}: {error}", path.display()))
            .ok();
        Self(Arc::new(std::sync::Mutex::new(file)))
    }

    fn write(&self, source: &str, text: &str) {
        use std::io::Write;
        if let Some(file) = self.0.lock().expect("guest log lock").as_mut() {
            let _ = writeln!(
                file,
                "{} [{source}] {text}",
                utc(std::time::SystemTime::now())
            );
        }
    }

    fn event(&self, event: &GuestEvent) {
        let phase = |phase: &Phase| match phase {
            Phase::Setup => "setup",
            Phase::Harness => "harness",
        };
        match event {
            GuestEvent::Phase { phase: p } => self.write(phase(p), "started"),
            GuestEvent::Log { phase: p, line } => self.write(phase(p), line),
            GuestEvent::Exit { phase: p, code } => {
                self.write(phase(p), &format!("exited with code {code:?}"))
            }
            GuestEvent::Error { message } => self.write("agent", message),
        }
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
    log: &GuestLog,
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
                log.write("factory", "stopping the session");
                let mut message = serde_json::to_string(&HostMessage::Stop).expect("serialises");
                message.push('\n');
                let _ = control.write_all(message.as_bytes()).await;
                deadline = Box::pin(tokio::time::sleep(SHUTDOWN_TIMEOUT));
            }
            () = &mut deadline => break,
            line = events.next_line() => match line {
                Ok(Some(line)) => match serde_json::from_str::<GuestEvent>(&line).inspect(|event| log.event(event)) {
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
