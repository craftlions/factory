//! Running sessions: one harness process each, fanned out to any number of
//! chat clients. Ended sessions are served from the harness's session file.

use crate::{
    collector, db,
    harness::{self, ChatEvent, ChatItem, Command, LaunchSpec, Resume, Update},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    fs, io,
    path::PathBuf,
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

enum Refusal {
    Removed(String),
    NotImplemented(String),
}

/// Decides whether a harness may be started at this isolation level.
///
/// Running a harness directly on the host was removed: harnesses execute shell
/// commands without asking, and the factory has no authentication. The microvm
/// runner does not exist yet, so for now every level is refused. The launch
/// code behind this check spawns on the host; it must go through the microvm
/// runner before any level is allowed here.
fn check_isolation(isolation: &str) -> Result<(), Refusal> {
    match isolation {
        "harness" => Err(Refusal::NotImplemented(
            "Harness isolation in a microvm is not implemented yet.".into(),
        )),
        "none" => Err(Refusal::Removed(
            "Sessions without isolation are no longer offered because they are not safe.".into(),
        )),
        other => Err(Refusal::NotImplemented(format!(
            "Isolation level {other} is not implemented yet."
        ))),
    }
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

pub struct Sessions {
    pool: SqlitePool,
    root: PathBuf,
    live: Mutex<HashMap<String, Arc<Live>>>,
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

impl Sessions {
    pub fn new(pool: SqlitePool, data_dir: &std::path::Path) -> Self {
        Self {
            pool,
            root: data_dir.join("sessions"),
            live: Mutex::new(HashMap::new()),
        }
    }

    fn state_dir(&self, id: &str) -> PathBuf {
        self.root.join(id).join("harness")
    }

    fn get(&self, id: &str) -> Option<Arc<Live>> {
        self.live
            .lock()
            .expect("live sessions lock")
            .get(id)
            .cloned()
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

        let id = db::insert_session(&self.pool, &request, collector::now())
            .await
            .map_err(CreateError::Database)?;
        let workspace = self.root.join(&id).join("workspace");
        let state_dir = self.state_dir(&id);
        let launched = fs::create_dir_all(&workspace)
            .and_then(|()| fs::create_dir_all(&state_dir))
            .and_then(|()| {
                adapter.launch(&LaunchSpec {
                    workspace: &workspace,
                    state_dir: &state_dir,
                    provider: &model.provider,
                    model: &model.id,
                    reasoning: &model.reasoning,
                    resume: Resume::No,
                })
            });
        let running = match launched {
            Ok(running) => running,
            Err(error) => {
                let note = format!("failed to start: {error}");
                let _ =
                    db::finish_session(&self.pool, &id, "failed", Some(&note), collector::now())
                        .await;
                return Err(CreateError::Launch(error));
            }
        };

        self.attach(&id, running, Vec::new());
        Ok(id)
    }

    fn attach(self: &Arc<Self>, id: &str, running: harness::Running, items: Vec<ChatItem>) {
        let (events, _) = broadcast::channel(1024);
        let live = Arc::new(Live {
            commands: running.commands,
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
        tokio::spawn(self.clone().pump(id.to_owned(), live, running.updates));
    }

    /// Starts the harness again for an ended session, in the same workspace and
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
        let workspace = self.root.join(id).join("workspace");
        let state_dir = self.state_dir(id);
        if !workspace.is_dir() || !state_dir.is_dir() {
            return Err(ResumeError::Conflict(
                "the session's directories no longer exist".into(),
            ));
        }
        if !db::reopen_session(&self.pool, id)
            .await
            .map_err(ResumeError::Database)?
        {
            return Err(ResumeError::Conflict(
                "the session is already running".into(),
            ));
        }

        // The harness does not replay old messages, so the live state starts
        // from what its session file holds.
        let session_file = session.harness_session_file.as_ref().map(PathBuf::from);
        let read_dir = state_dir.clone();
        let read_file = session_file.clone();
        let read = move || adapter.read(&read_dir, read_file.as_deref());
        let started = match tokio::task::spawn_blocking(read).await {
            Ok(Ok(items)) => adapter
                .launch(&LaunchSpec {
                    workspace: &workspace,
                    state_dir: &state_dir,
                    provider,
                    model,
                    reasoning,
                    resume: session_file.as_deref().map_or(Resume::Newest, Resume::File),
                })
                .map(|running| (running, items)),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(io::Error::other(error)),
        };
        match started {
            Ok((running, items)) => {
                self.attach(id, running, items);
                Ok(())
            }
            Err(error) => {
                let note = format!("failed to restart: {error}");
                let _ = db::finish_session(&self.pool, id, "failed", Some(&note), collector::now())
                    .await;
                Err(ResumeError::Launch(error))
            }
        }
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

        let state_dir = self.state_dir(id);
        let session_file = session.harness_session_file.as_ref().map(PathBuf::from);
        let items = match harness::by_id(&session.kind) {
            Some(adapter) if state_dir.is_dir() => tokio::task::spawn_blocking(move || {
                adapter.read(&state_dir, session_file.as_deref())
            })
            .await
            .map_err(io::Error::other)??,
            _ => Vec::new(),
        };
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
