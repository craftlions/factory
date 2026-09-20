//! Running sessions: one harness process each, inside its own microvm, fanned
//! out to any number of chat clients. Ended sessions are served from the
//! harness's session file, copied out of the session's disk.

use crate::{
    collector, db,
    guest_protocol::GuestFile,
    harness::{self, ChatEvent, ChatItem, Command, Harness, LaunchSpec, ModelInfo, Resume, Update},
    microvm::{self, Microvm, VmRequest, proxy},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, mpsc};

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub harness: String,
    pub isolation: String,
    pub workdir: Workdir,
    pub model: ModelChoice,
}

#[derive(Debug, Deserialize)]
pub struct Workdir {
    pub kind: String,
}

#[derive(Debug, Deserialize)]
pub struct ModelChoice {
    pub provider: String,
    pub id: String,
    pub reasoning: String,
}

#[derive(Debug)]
pub enum CreateError {
    /// A valid choice that has no implementation yet.
    Unsupported(String),
    Invalid(String),
    Launch(io::Error),
    Database(sqlx::Error),
}

#[derive(Debug)]
pub enum ResumeError {
    NotFound,
    /// Already running, or a row from before sessions recorded their configuration.
    Conflict(String),
    Launch(io::Error),
    Database(sqlx::Error),
}

/// Everything a chat client needs to draw the conversation from scratch.
#[derive(Debug, Serialize)]
pub struct Snapshot {
    pub session: db::Session,
    pub items: Vec<ChatItem>,
    /// Events of the message and tool calls still in flight, in order.
    pub pending: Vec<ChatEvent>,
    pub working: bool,
}

/// Decides whether a harness may be started at this isolation level. Only a
/// microvm is offered: harnesses execute shell commands without asking, so they
/// never run directly on the host.
fn check_isolation(isolation: &str) -> Result<(), Refusal> {
    match isolation {
        "harness" => Ok(()),
        "none" => Err(Refusal::Removed(
            "Sessions without isolation are no longer offered because they are not safe.".into(),
        )),
        other => Err(Refusal::NotImplemented(format!(
            "Isolation level {other} is not implemented yet."
        ))),
    }
}

enum Refusal {
    Removed(String),
    NotImplemented(String),
}

/// What one start of a harness needs, owned so it can move into a task.
struct Launch {
    adapter: &'static dyn Harness,
    provider: String,
    model: String,
    reasoning: String,
    /// The harness's session file inside the guest, when continuing one.
    resume: Option<Option<String>>,
    /// API hosts the harness may reach.
    hosts: Vec<String>,
}

struct LiveState {
    items: Vec<ChatItem>,
    pending: Vec<ChatEvent>,
    working: bool,
}

struct Live {
    commands: mpsc::UnboundedSender<Command>,
    events: broadcast::Sender<ChatEvent>,
    // Held while broadcasting, so a snapshot and its subscription never miss
    // or repeat an event.
    state: Mutex<LiveState>,
}

fn is_tool_event(event: &ChatEvent, tool_id: &str) -> bool {
    matches!(event,
        ChatEvent::ToolStart { id, .. } | ChatEvent::ToolUpdate { id, .. } | ChatEvent::ToolEnd { id, .. }
        if id == tool_id)
}

impl LiveState {
    fn apply(&mut self, event: &ChatEvent) {
        match event {
            ChatEvent::Working { working } => self.working = *working,
            ChatEvent::Notice { text } => self.items.push(ChatItem::Notice {
                text: text.clone(),
                ts: collector::now() * 1000,
            }),
            ChatEvent::MessageEnd { item } => {
                match item {
                    ChatItem::Assistant { .. } => self.pending.retain(|e| {
                        !matches!(
                            e,
                            ChatEvent::MessageStart
                                | ChatEvent::BlockStart { .. }
                                | ChatEvent::BlockDelta { .. }
                        )
                    }),
                    ChatItem::ToolResult { tool_call_id, .. } => {
                        self.pending.retain(|e| !is_tool_event(e, tool_call_id));
                    }
                    _ => {}
                }
                self.items.push(item.clone());
            }
            // A tool's partial output is cumulative, so only the latest matters.
            ChatEvent::ToolUpdate { id, .. } => {
                self.pending
                    .retain(|e| !matches!(e, ChatEvent::ToolUpdate { id: old, .. } if old == id));
                self.pending.push(event.clone());
            }
            _ => self.pending.push(event.clone()),
        }
    }
}

pub struct Sessions {
    pool: SqlitePool,
    root: PathBuf,
    /// pi's configuration and credentials, copied into every guest.
    pi_config: PathBuf,
    microvm: Microvm,
    /// One catalog vm at a time: they share a disk.
    catalog_vm: tokio::sync::Mutex<()>,
    /// The last model list per harness, which knows each model's API host.
    catalog: Mutex<HashMap<String, Vec<ModelInfo>>>,
    live: Mutex<HashMap<String, Arc<Live>>>,
}

impl Sessions {
    pub fn new(pool: SqlitePool, data_dir: &Path, microvm: Microvm) -> Self {
        Self {
            pool,
            root: data_dir.join("sessions"),
            pi_config: data_dir.join("pi").join("agent"),
            microvm,
            catalog_vm: tokio::sync::Mutex::new(()),
            catalog: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
        }
    }

    fn dir(&self, id: &str) -> PathBuf {
        self.root.join(id)
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.dir(id).join("harness")
    }

    fn get(&self, id: &str) -> Option<Arc<Live>> {
        self.live
            .lock()
            .expect("live sessions lock")
            .get(id)
            .cloned()
    }

    /// The harness's configuration files for the guest. Credentials are copied
    /// in, so code running in the guest can read them.
    fn guest_files(&self) -> io::Result<Vec<GuestFile>> {
        let auth = self.pi_config.join("auth.json");
        if !auth.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!(
                    "pi has no credentials yet. Place pi's auth.json at {}.",
                    auth.display()
                ),
            ));
        }
        let mut files = Vec::new();
        for name in ["auth.json", "models.json", "settings.json"] {
            match fs::read_to_string(self.pi_config.join(name)) {
                Ok(content) => files.push(GuestFile {
                    path: format!("{}/.pi/agent/{name}", self.microvm.home()),
                    mode: 0o600,
                    content,
                }),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(files)
    }

    /// Asks the harness, in a throwaway vm, which models it can use.
    pub async fn models(&self, harness_id: &str) -> Option<io::Result<Vec<ModelInfo>>> {
        let adapter = harness::by_id(harness_id)?;
        let result = async {
            let invocation = adapter.catalog_invocation(None);
            let process = self.catalog_process(&invocation).await?;
            adapter.models(process).await
        }
        .await;
        if let Ok(models) = &result {
            let mut catalog = self.catalog.lock().expect("catalog lock");
            catalog.insert(harness_id.to_owned(), models.clone());
        }
        Some(result)
    }

    /// Asks the harness, in a throwaway vm, which reasoning levels a model has.
    pub async fn reasoning_levels(
        &self,
        harness_id: &str,
        provider: &str,
        model: &str,
    ) -> Option<io::Result<Vec<String>>> {
        let adapter = harness::by_id(harness_id)?;
        Some(
            async {
                let invocation = adapter.catalog_invocation(Some((provider, model)));
                let process = self.catalog_process(&invocation).await?;
                adapter
                    .reasoning_levels(process, provider.to_owned(), model.to_owned())
                    .await
            }
            .await,
        )
    }

    /// Catalog vms reuse one disk, so tools are only installed the first time.
    async fn catalog_process(
        &self,
        invocation: &harness::Invocation,
    ) -> io::Result<harness::Process> {
        let _one_at_a_time = self.catalog_vm.lock().await;
        let dir = self.microvm.assets_dir.join("catalog");
        tokio::fs::create_dir_all(&dir).await?;
        let (notices, _) = mpsc::unbounded_channel();
        let request = VmRequest {
            dir: &dir,
            invocation,
            files: self.guest_files()?,
            hosts: Vec::new(),
            notices,
        };
        self.microvm.start(request).await
    }

    /// The API host of a model, from the harness's own model list.
    async fn api_hosts(
        &self,
        harness_id: &str,
        provider: &str,
        model: &str,
    ) -> io::Result<Vec<String>> {
        let find = |models: &[ModelInfo]| {
            models
                .iter()
                .find(|m| m.provider == provider && m.id == model)
                .map(|m| m.base_url.clone())
        };
        let cached = self
            .catalog
            .lock()
            .expect("catalog lock")
            .get(harness_id)
            .and_then(|m| find(m));
        let base_url = match cached {
            Some(base_url) => base_url,
            None => {
                let models = self.models(harness_id).await.expect("the harness exists")?;
                find(&models).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("{harness_id} does not list {provider}/{model}"),
                    )
                })?
            }
        };
        let host = base_url
            .as_deref()
            .and_then(proxy::host_of)
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("{harness_id} reports no API address for {provider}/{model}"),
                )
            })?;
        Ok(vec![host])
    }

    pub async fn create(self: &Arc<Self>, request: CreateRequest) -> Result<String, CreateError> {
        let Some(adapter) = harness::by_id(&request.harness) else {
            return Err(CreateError::Unsupported(format!(
                "The {} harness is not implemented yet.",
                request.harness
            )));
        };
        check_isolation(&request.isolation).map_err(|refusal| match refusal {
            Refusal::Removed(message) => CreateError::Invalid(message),
            Refusal::NotImplemented(message) => CreateError::Unsupported(message),
        })?;
        if request.workdir.kind != "empty" {
            return Err(CreateError::Unsupported(format!(
                "Working directory kind {} is not implemented yet.",
                request.workdir.kind
            )));
        }
        let model = &request.model;
        if model.provider.is_empty() || model.id.is_empty() || model.reasoning.is_empty() {
            return Err(CreateError::Invalid(
                "provider, model and reasoning are required".into(),
            ));
        }
        self.guest_files().map_err(CreateError::Launch)?;
        let hosts = self
            .api_hosts(&request.harness, &model.provider, &model.id)
            .await
            .map_err(CreateError::Launch)?;

        let id = db::insert_session(&self.pool, &request, collector::now())
            .await
            .map_err(CreateError::Database)?;
        if let Err(error) = fs::create_dir_all(self.dir(&id)) {
            let note = format!("failed to start: {error}");
            let _ =
                db::finish_session(&self.pool, &id, "failed", Some(&note), collector::now()).await;
            return Err(CreateError::Launch(error));
        }
        let launch = Launch {
            adapter,
            provider: model.provider.clone(),
            model: model.id.clone(),
            reasoning: model.reasoning.clone(),
            resume: None,
            hosts,
        };
        self.attach(&id, launch, Vec::new());
        Ok(id)
    }

    /// Registers the session as live and starts its vm in the background. The
    /// chat is usable at once: setup progress arrives as notices, and messages
    /// sent meanwhile wait for the harness.
    fn attach(self: &Arc<Self>, id: &str, launch: Launch, items: Vec<ChatItem>) {
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1024);
        let live = Arc::new(Live {
            commands,
            events,
            state: Mutex::new(LiveState {
                items,
                pending: Vec::new(),
                working: false,
            }),
        });
        self.live
            .lock()
            .expect("live sessions lock")
            .insert(id.to_owned(), live.clone());
        tokio::spawn(
            self.clone()
                .run(id.to_owned(), launch, command_rx, update_tx),
        );
        tokio::spawn(self.clone().pump(id.to_owned(), live, updates));
    }

    async fn run(
        self: Arc<Self>,
        id: String,
        launch: Launch,
        mut commands: mpsc::UnboundedReceiver<Command>,
        updates: mpsc::UnboundedSender<Update>,
    ) {
        let state_dir = self.microvm.state_dir();
        let spec = LaunchSpec {
            state_dir: &state_dir,
            provider: &launch.provider,
            model: &launch.model,
            reasoning: &launch.reasoning,
            resume: match &launch.resume {
                None => Resume::No,
                Some(Some(file)) => Resume::File(file),
                Some(None) => Resume::Newest,
            },
        };
        let invocation = launch.adapter.invocation(&spec);

        let (notices, mut notice_rx) = mpsc::unbounded_channel::<String>();
        let notice_updates = updates.clone();
        tokio::spawn(async move {
            while let Some(text) = notice_rx.recv().await {
                if notice_updates
                    .send(Update::Event(ChatEvent::Notice { text }))
                    .is_err()
                {
                    break;
                }
            }
        });
        let _ = notices.send("Starting the session's microvm.".into());

        let dir = self.dir(&id);
        let files = match self.guest_files() {
            Ok(files) => files,
            Err(error) => {
                let _ = updates.send(Update::Exited {
                    failure: Some(error.to_string()),
                });
                return;
            }
        };
        let request = VmRequest {
            dir: &dir,
            invocation: &invocation,
            files,
            hosts: launch.hosts.clone(),
            notices,
        };

        // Messages sent during setup are kept for the harness; a stop ends setup.
        let mut early = Vec::new();
        let started = {
            let start = self.microvm.start(request);
            tokio::pin!(start);
            loop {
                tokio::select! {
                    started = &mut start => break Some(started),
                    command = commands.recv() => match command {
                        Some(Command::Stop) | None => break None,
                        Some(command) => early.push(command),
                    },
                }
            }
        };
        match started {
            // Dropping the start future killed the half-started vm.
            None => {
                let _ = updates.send(Update::Exited { failure: None });
            }
            Some(Err(error)) => {
                let _ = updates.send(Update::Exited {
                    failure: Some(error.to_string()),
                });
            }
            Some(Ok(process)) => {
                let (forward, adapter_commands) = mpsc::unbounded_channel();
                for command in early {
                    let _ = forward.send(command);
                }
                launch
                    .adapter
                    .attach(process, &spec, adapter_commands, updates);
                while let Some(command) = commands.recv().await {
                    if forward.send(command).is_err() {
                        break;
                    }
                }
            }
        }
    }

    /// Starts the harness again for an ended session, on the same disk and
    /// continuing the conversation in its session file.
    pub async fn resume(self: &Arc<Self>, id: &str) -> Result<(), ResumeError> {
        let session = db::session(&self.pool, id)
            .await
            .map_err(ResumeError::Database)?
            .ok_or(ResumeError::NotFound)?;
        let adapter = harness::by_id(&session.kind);
        let (Some(adapter), Some(provider), Some(model), Some(reasoning)) = (
            adapter,
            &session.provider,
            &session.model,
            &session.reasoning,
        ) else {
            return Err(ResumeError::Conflict(
                "this session has no recorded configuration to restart from".into(),
            ));
        };
        match check_isolation(session.isolation.as_deref().unwrap_or("none")) {
            Ok(()) => {}
            Err(Refusal::Removed(message) | Refusal::NotImplemented(message)) => {
                return Err(ResumeError::Conflict(message));
            }
        }
        if !self.dir(id).join("disk.ext4").is_file() {
            return Err(ResumeError::Conflict(
                "the session's disk no longer exists".into(),
            ));
        }
        let hosts = self
            .api_hosts(&session.kind, provider, model)
            .await
            .map_err(ResumeError::Launch)?;
        // The harness does not replay old messages, so the live state starts
        // from what its session file holds.
        let items = self
            .transcript(&session)
            .await
            .map_err(ResumeError::Launch)?;
        if !db::reopen_session(&self.pool, id)
            .await
            .map_err(ResumeError::Database)?
        {
            return Err(ResumeError::Conflict(
                "the session is already running".into(),
            ));
        }
        let launch = Launch {
            adapter,
            provider: provider.clone(),
            model: model.clone(),
            reasoning: reasoning.clone(),
            resume: Some(session.harness_session_file.clone()),
            hosts,
        };
        self.attach(id, launch, items);
        Ok(())
    }

    /// An ended session's conversation, from the harness's own session file.
    async fn transcript(&self, session: &db::Session) -> io::Result<Vec<ChatItem>> {
        let Some(adapter) = harness::by_id(&session.kind) else {
            return Ok(Vec::new());
        };
        // A vm that died with the factory has not been copied out yet.
        self.microvm.extract(&self.dir(&session.id)).await?;
        let state_dir = self.state_dir(&session.id);
        if !state_dir.is_dir() {
            return Ok(Vec::new());
        }
        // The recorded path is the guest's; the file has the same name here.
        let file = session
            .harness_session_file
            .as_deref()
            .and_then(|guest| microvm::extracted(&state_dir, guest))
            .filter(|file| file.is_file());
        tokio::task::spawn_blocking(move || adapter.read(&state_dir, file.as_deref()))
            .await
            .map_err(io::Error::other)?
    }

    async fn pump(
        self: Arc<Self>,
        id: String,
        live: Arc<Live>,
        mut updates: mpsc::UnboundedReceiver<Update>,
    ) {
        let mut failure = Some("the harness adapter stopped unexpectedly".to_owned());
        while let Some(update) = updates.recv().await {
            match update {
                Update::Event(event) => {
                    let mut state = live.state.lock().expect("live state lock");
                    state.apply(&event);
                    let _ = live.events.send(event);
                }
                Update::Identity {
                    session_id,
                    session_file,
                } => {
                    let saved = db::set_harness_session(
                        &self.pool,
                        &id,
                        &session_id,
                        session_file.as_deref(),
                    );
                    if let Err(error) = saved.await {
                        eprintln!("failed to record harness session of {id}: {error}");
                    }
                }
                Update::Exited { failure: reported } => {
                    failure = reported;
                    break;
                }
            }
        }
        // Copy the conversation out of the disk before clients reload it.
        if let Err(error) = self.microvm.extract(&self.dir(&id)).await {
            eprintln!("failed to extract session {id}: {error}");
        }
        let status = if failure.is_some() {
            "failed"
        } else {
            "completed"
        };
        if let Err(error) = db::finish_session(
            &self.pool,
            &id,
            status,
            failure.as_deref(),
            collector::now(),
        )
        .await
        {
            eprintln!("failed to close session {id}: {error}");
        }
        // Dropping the last sender ends every client's stream; they then
        // reload the ended session from its file.
        self.live.lock().expect("live sessions lock").remove(&id);
    }

    /// Sends a command to a running session. `false` means it is not running.
    pub fn send(&self, id: &str, command: Command) -> bool {
        self.get(id)
            .is_some_and(|live| live.commands.send(command).is_ok())
    }

    /// The conversation so far, plus a live subscription while it runs.
    pub async fn open(
        &self,
        id: &str,
    ) -> Result<Option<(Snapshot, Option<broadcast::Receiver<ChatEvent>>)>, io::Error> {
        let Some(session) = db::session(&self.pool, id)
            .await
            .map_err(io::Error::other)?
        else {
            return Ok(None);
        };
        if let Some(live) = self.get(id) {
            let state = live.state.lock().expect("live state lock");
            let snapshot = Snapshot {
                session,
                items: state.items.clone(),
                pending: state.pending.clone(),
                working: state.working,
            };
            return Ok(Some((snapshot, Some(live.events.subscribe()))));
        }

        let items = self.transcript(&session).await?;
        Ok(Some((
            Snapshot {
                session,
                items,
                pending: Vec::new(),
                working: false,
            },
            None,
        )))
    }
}
