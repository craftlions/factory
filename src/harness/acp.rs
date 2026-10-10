//! Agent Client Protocol adapter, for any harness that speaks ACP: JSON-RPC 2.0
//! as newline-delimited JSON over stdio (agentclientprotocol.com). The factory
//! is the client. It offers the agent no file system or terminal of its own, so
//! the agent uses the guest's, and it grants permission requests itself: the
//! chat has no dialogs, and the harness runs isolated anyway.
//!
//! What differs between agents comes from a [`Profile`]. Ended sessions are
//! read from the agent's own transcript, which ACP does not describe, so each
//! harness brings its own [`super::TranscriptReader`].

use super::{
    Block, BlockKind, ChatEvent, ChatItem, Command, LaunchSpec, Process, Resume, Trace, Update,
    clip,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

const PROTOCOL_VERSION: u64 = 1;
/// An extension that adds a message to the running turn instead of queueing a
/// new one. Agents advertise it in `_meta.steering` of their initialize answer.
const STEER_METHOD: &str = "_session/steering";
/// How long an agent may take to open its session before it counts as stuck.
const OPEN_TIMEOUT: Duration = Duration::from_secs(120);

/// What sets one ACP agent apart.
pub struct Profile {
    /// The agent's name in notices.
    pub name: &'static str,
    /// The session mode that runs tools without asking, when the agent has one.
    pub unattended_mode: Option<&'static str>,
    /// The agent's transcript of a session inside the guest, from the state
    /// directory, the working directory and the session id.
    pub session_file: fn(&str, &str, &str) -> Option<String>,
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as i64)
}

fn client_capabilities() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
    })
}

// ---- JSON-RPC connection ----

type Reply = Result<Value, String>;

enum Incoming {
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    Notification {
        method: String,
        params: Value,
    },
}

struct Connection {
    /// Lines for the agent's stdin; the empty line closes it.
    lines: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Reply>>>,
    trace: Trace,
}

/// One line about a message for the trace: its method and id, never its
/// content. Streaming updates are left out.
fn describe(message: &Value) -> Option<String> {
    let id = match &message["id"] {
        Value::Null => String::new(),
        id => format!(" #{id}"),
    };
    match (message["method"].as_str(), message.get("error")) {
        (Some("session/update"), _) => None,
        (Some(method), _) => Some(format!("{method}{id}")),
        (None, Some(error)) => Some(format!(
            "error{id}: {}",
            error["message"].as_str().unwrap_or("unknown error")
        )),
        (None, None) => Some(format!("answer{id}")),
    }
}

impl Connection {
    fn send(&self, message: Value) -> bool {
        if let Some(line) = describe(&message) {
            (self.trace)(&format!("→ {line}"));
        }
        self.lines.send(message.to_string()).is_ok()
    }

    async fn request(&self, method: &str, params: Value) -> Reply {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, answered) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id, answer);
        let message = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if !self.send(message) {
            return Err("the agent went away".into());
        }
        answered
            .await
            .unwrap_or_else(|_| Err(format!("the agent went away before answering {method}")))
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    fn reply(&self, id: &Value, result: Value) {
        self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}));
    }

    fn refuse(&self, id: &Value, message: &str) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": message},
        }));
    }

    fn settle(&self, message: &Value) {
        let Some(id) = message["id"].as_u64() else {
            return;
        };
        let Some(answer) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let reply = match message.get("error") {
            Some(error) => Err(error["message"]
                .as_str()
                .unwrap_or("unknown error")
                .to_owned()),
            None => Ok(message["result"].clone()),
        };
        let _ = answer.send(reply);
    }

    /// Closes the agent's stdin, which is how an ACP agent is told to exit.
    fn close(&self) {
        let _ = self.lines.send(String::new());
    }
}

/// Starts writing to the agent and reading from it. Responses settle their
/// requests; every other message goes to `incoming`. The returned task ends
/// with the agent's stdout.
fn connect(
    mut stdin: Box<dyn AsyncWrite + Send + Unpin>,
    stdout: Box<dyn AsyncRead + Send + Unpin>,
    trace: Trace,
    mut incoming: impl FnMut(&Connection, Incoming) + Send + 'static,
) -> (Arc<Connection>, JoinHandle<()>) {
    let (lines, mut line_rx) = mpsc::unbounded_channel::<String>();
    let writer_trace = trace.clone();
    tokio::spawn(async move {
        while let Some(line) = line_rx.recv().await {
            if line.is_empty() {
                // Dropping stdin sends EOF.
                writer_trace("closing the agent's stdin");
                break;
            }
            if let Err(error) = write_line(&mut stdin, &line).await {
                writer_trace(&format!("writing to the agent failed: {error}"));
                break;
            }
        }
    });

    let connection = Arc::new(Connection {
        lines,
        next_id: AtomicU64::new(0),
        pending: Mutex::new(HashMap::new()),
        trace: trace.clone(),
    });
    let reader_connection = connection.clone();
    let reader = tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                let shown: String = line.chars().take(200).collect();
                trace(&format!("← not a protocol message: {shown}"));
                continue;
            };
            if let Some(line) = describe(&message) {
                trace(&format!("← {line}"));
            }
            let method = message["method"].as_str().map(str::to_owned);
            match (message.get("id"), method) {
                (Some(id), Some(method)) => incoming(
                    &reader_connection,
                    Incoming::Request {
                        id: id.clone(),
                        method,
                        params: message["params"].clone(),
                    },
                ),
                (None, Some(method)) => incoming(
                    &reader_connection,
                    Incoming::Notification {
                        method,
                        params: message["params"].clone(),
                    },
                ),
                (Some(_), None) => reader_connection.settle(&message),
                (None, None) => {}
            }
        }
        trace("the agent's stdout closed");
        // Waiting requests fail once their answers are dropped.
        reader_connection
            .pending
            .lock()
            .expect("pending lock")
            .clear();
    });
    (connection, reader)
}

async fn write_line(stdin: &mut (impl AsyncWrite + Unpin), line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

// ---- session config options ----

/// Session config options carry a category for the choices clients know.
fn option<'a>(options: &'a Value, category: &str) -> Option<&'a Value> {
    options
        .as_array()?
        .iter()
        .find(|option| option["category"] == category)
}

/// The values of a select option, from flat or grouped choices.
fn values(option: &Value) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for choice in option["options"].as_array().into_iter().flatten() {
        let nested = choice["options"].as_array();
        for choice in nested.map_or_else(|| vec![choice], |group| group.iter().collect()) {
            if let Some(value) = choice["value"].as_str() {
                let name = choice["name"].as_str().unwrap_or(value);
                found.push((value.to_owned(), name.to_owned()));
            }
        }
    }
    found
}

// ---- live session ----

/// A tool call the agent announced but has not started yet. Its input may
/// still be streaming in, so it is shown once it runs.
struct Announced {
    id: String,
    name: String,
    input: Value,
}

/// Turns the agent's session updates into chat events. Text streams as it
/// arrives; announced tool calls join the message once one of them runs.
#[derive(Default)]
struct Turn {
    /// The assistant message in flight: the agent's id for it, and its blocks.
    message: Option<(Option<String>, Vec<Block>)>,
    announced: Vec<Announced>,
    /// Tool calls shown and still running, with their names.
    running: HashMap<String, String>,
    /// Prompts sent and not answered yet.
    prompts: usize,
}

fn tool_name(call: &Value) -> String {
    call["name"]
        .as_str()
        .or_else(|| call["title"].as_str())
        .unwrap_or("tool")
        .to_owned()
}

fn content_text(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(str::to_owned),
        "image" => Some("[image]".into()),
        "resource_link" => block["uri"].as_str().map(str::to_owned),
        "resource" => block["resource"]["text"]
            .as_str()
            .or_else(|| block["resource"]["uri"].as_str())
            .map(str::to_owned),
        _ => None,
    }
}

/// A tool's output: its raw output when that is text, else what it reported
/// for display.
fn tool_output(update: &Value) -> String {
    if let Some(raw) = update["rawOutput"].as_str().filter(|raw| !raw.is_empty()) {
        return clip(raw.to_owned());
    }
    let shown: Vec<String> = update["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match item["type"].as_str()? {
            "content" => content_text(&item["content"]),
            "diff" => Some(format!(
                "{}\n{}",
                item["path"].as_str().unwrap_or("file"),
                item["newText"].as_str().unwrap_or("")
            )),
            _ => None,
        })
        .collect();
    if shown.is_empty() {
        match &update["rawOutput"] {
            Value::Null | Value::String(_) => String::new(),
            raw => clip(raw.to_string()),
        }
    } else {
        clip(shown.join("\n"))
    }
}

impl Turn {
    fn close_message(&mut self, stop_reason: Option<String>, out: &mut Vec<ChatEvent>) {
        let Some((_, blocks)) = self.message.take() else {
            return;
        };
        out.push(ChatEvent::MessageEnd {
            item: ChatItem::Assistant {
                blocks,
                model: None,
                stop_reason,
                error: None,
                ts: now(),
            },
        });
    }

    /// Ends the message with the announced tool calls in it, and starts them.
    fn start_tools(&mut self, out: &mut Vec<ChatEvent>) {
        if self.announced.is_empty() {
            return;
        }
        let announced = std::mem::take(&mut self.announced);
        let (_, blocks) = self.message.get_or_insert_with(Default::default);
        blocks.extend(announced.iter().map(|tool| Block::ToolCall {
            id: tool.id.clone(),
            name: tool.name.clone(),
            arguments: tool.input.clone(),
        }));
        self.close_message(None, out);
        for tool in announced {
            out.push(ChatEvent::ToolStart {
                id: tool.id.clone(),
                name: tool.name.clone(),
                arguments: tool.input,
            });
            self.running.insert(tool.id, tool.name);
        }
    }

    fn chunk(&mut self, kind: BlockKind, update: &Value, out: &mut Vec<ChatEvent>) {
        let Some(text) = content_text(&update["content"]).filter(|text| !text.is_empty()) else {
            return;
        };
        // Text after tool calls means their input is complete.
        self.start_tools(out);
        let message_id = update["messageId"].as_str();
        if let Some((Some(current), _)) = &self.message
            && message_id.is_some_and(|id| id != current.as_str())
        {
            self.close_message(None, out);
        }
        let (_, blocks) = self.message.get_or_insert_with(|| {
            out.push(ChatEvent::MessageStart);
            (message_id.map(str::to_owned), Vec::new())
        });
        match (blocks.last_mut(), kind) {
            (Some(Block::Text { text: so_far }), BlockKind::Text)
            | (Some(Block::Thinking { text: so_far }), BlockKind::Thinking) => {
                so_far.push_str(&text);
            }
            _ => {
                blocks.push(match kind {
                    BlockKind::Thinking => Block::Thinking { text: text.clone() },
                    _ => Block::Text { text: text.clone() },
                });
                out.push(ChatEvent::BlockStart {
                    index: blocks.len() - 1,
                    kind,
                    id: None,
                    name: None,
                });
            }
        }
        out.push(ChatEvent::BlockDelta {
            index: blocks.len() - 1,
            delta: text,
        });
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<ChatEvent>) {
        let Some(id) = call["toolCallId"].as_str() else {
            return;
        };
        let known = self.running.contains_key(id) || self.announced.iter().any(|t| t.id == id);
        if !known {
            self.announced.push(Announced {
                id: id.to_owned(),
                name: tool_name(call),
                input: match &call["rawInput"] {
                    Value::Null => json!({}),
                    input => input.clone(),
                },
            });
        }
        self.tool_update(call, false, out);
    }

    /// `running` says the tool runs now, as when it asks for permission.
    fn tool_update(&mut self, update: &Value, running: bool, out: &mut Vec<ChatEvent>) {
        let Some(id) = update["toolCallId"].as_str() else {
            return;
        };
        if let Some(tool) = self.announced.iter_mut().find(|tool| tool.id == id) {
            if !update["rawInput"].is_null() {
                tool.input = update["rawInput"].clone();
            }
            // Agents report display content and results only once the input
            // is complete.
            let status = update["status"].as_str();
            let has_content = update["content"]
                .as_array()
                .is_some_and(|content| !content.is_empty());
            let started = running
                || matches!(status, Some("in_progress" | "completed" | "failed"))
                || has_content
                || !update["rawOutput"].is_null();
            if started {
                self.start_tools(out);
            }
        }
        let Some(name) = self.running.get(id).cloned() else {
            return;
        };
        match update["status"].as_str() {
            Some(status @ ("completed" | "failed")) => {
                self.running.remove(id);
                self.end_tool(id, name, tool_output(update), status == "failed", out);
            }
            _ if update["rawOutput"].is_string() => out.push(ChatEvent::ToolUpdate {
                id: id.to_owned(),
                output: tool_output(update),
            }),
            _ => {}
        }
    }

    fn end_tool(
        &mut self,
        id: &str,
        name: String,
        output: String,
        is_error: bool,
        out: &mut Vec<ChatEvent>,
    ) {
        out.push(ChatEvent::ToolEnd {
            id: id.to_owned(),
            output: output.clone(),
            is_error,
        });
        out.push(ChatEvent::MessageEnd {
            item: ChatItem::ToolResult {
                tool_call_id: id.to_owned(),
                tool_name: name,
                text: output,
                is_error,
                ts: now(),
            },
        });
    }

    fn update(&mut self, update: &Value, out: &mut Vec<ChatEvent>) {
        match update["sessionUpdate"].as_str().unwrap_or("") {
            "agent_message_chunk" => self.chunk(BlockKind::Text, update, out),
            "agent_thought_chunk" => self.chunk(BlockKind::Thinking, update, out),
            "tool_call" => self.tool_call(update, out),
            "tool_call_update" => self.tool_update(update, false, out),
            _ => {}
        }
    }

    fn prompt_sent(&mut self, out: &mut Vec<ChatEvent>) {
        self.prompts += 1;
        if self.prompts == 1 {
            out.push(ChatEvent::Working { working: true });
        }
    }

    /// A prompt was answered. Whatever the turn left open ends with it.
    fn prompt_ended(&mut self, stop_reason: Option<&str>, out: &mut Vec<ChatEvent>) {
        self.start_tools(out);
        // The chat marks an interrupted message as aborted.
        let stop_reason = stop_reason.map(|reason| match reason {
            "cancelled" => "aborted".to_owned(),
            other => other.to_owned(),
        });
        if stop_reason.as_deref() == Some("aborted") {
            self.message.get_or_insert_with(Default::default);
        }
        self.close_message(stop_reason, out);
        let unfinished: Vec<(String, String)> = self.running.drain().collect();
        for (id, name) in unfinished {
            let output = "The turn ended before the tool finished.".to_owned();
            self.end_tool(&id, name, output, true, out);
        }
        self.prompts = self.prompts.saturating_sub(1);
        if self.prompts == 0 {
            out.push(ChatEvent::Working { working: false });
        }
    }
}

fn emit(updates: &mpsc::UnboundedSender<Update>, events: Vec<ChatEvent>) {
    for event in events {
        let _ = updates.send(Update::Event(event));
    }
}

/// Shows a user's message as soon as it arrives, even while the agent is
/// still opening its session; ACP agents do not echo prompts back.
fn echo(updates: &mpsc::UnboundedSender<Update>, command: &Command) {
    if let Command::Prompt(text) = command {
        emit(
            updates,
            vec![ChatEvent::MessageEnd {
                item: ChatItem::User {
                    text: text.clone(),
                    ts: now(),
                },
            }],
        );
    }
}

/// Grants a permission request with the first option that allows once, else
/// one that allows always. `None` when the agent offered no way to allow.
fn grant(options: &Value) -> Option<Value> {
    let options = options.as_array()?;
    ["allow_once", "allow_always"].iter().find_map(|kind| {
        let choice = options.iter().find(|option| option["kind"] == *kind)?;
        Some(json!({"outcome": {"outcome": "selected", "optionId": choice["optionId"]}}))
    })
}

/// What starting a session needs, owned so it can move into a task.
struct Start {
    state_dir: String,
    cwd: String,
    model: String,
    reasoning: String,
    resume: Option<Option<String>>,
}

struct Session {
    id: String,
    steering: bool,
}

/// Opens or continues the session and applies the launch choices to it.
/// Choices the agent does not take become notices in `out`.
async fn open_session(
    connection: &Connection,
    profile: &Profile,
    start: &Start,
    out: &mut Vec<ChatEvent>,
) -> Result<Session, String> {
    let initialized = connection
        .request("initialize", client_capabilities())
        .await?;
    let steering = initialized["_meta"]["steering"]["supported"] == true;
    let name = profile.name;
    let fresh = json!({"cwd": start.cwd, "mcpServers": []});

    let previous = match &start.resume {
        None => None,
        Some(Some(id)) => Some(id.clone()),
        Some(None) => connection
            .request("session/list", json!({"cwd": start.cwd}))
            .await
            .ok()
            .and_then(|listed| {
                listed["sessions"][0]["sessionId"]
                    .as_str()
                    .map(str::to_owned)
            }),
    };
    let mut opened = None;
    if let Some(id) = previous {
        let params = json!({"sessionId": id, "cwd": start.cwd, "mcpServers": []});
        match connection.request("session/resume", params).await {
            Ok(resumed) => opened = Some((id, resumed["configOptions"].clone())),
            Err(error) => out.push(ChatEvent::Notice {
                text: format!(
                    "{name} could not continue its earlier session ({error}), so it starts a new one."
                ),
            }),
        }
    }
    let (id, mut options) = match opened {
        Some(opened) => opened,
        None => {
            let created = connection.request("session/new", fresh).await?;
            let id = created["sessionId"]
                .as_str()
                .ok_or("the agent named no session")?
                .to_owned();
            (id, created["configOptions"].clone())
        }
    };

    let choices = [
        ("mode", "mode", profile.unattended_mode),
        ("model", "model", Some(start.model.as_str())),
        (
            "reasoning level",
            "thought_level",
            Some(start.reasoning.as_str()),
        ),
    ];
    for (what, category, value) in choices {
        let Some(value) = value else {
            continue;
        };
        let Some(choice) = option(&options, category) else {
            // An agent without levels reports the single level "default".
            if category != "mode" && value != "default" {
                out.push(ChatEvent::Notice {
                    text: format!("{name} offers no {what} choice, so {value} was not applied."),
                });
            }
            continue;
        };
        if choice["currentValue"] == value {
            continue;
        }
        if !values(choice).iter().any(|(offered, _)| offered == value) {
            out.push(ChatEvent::Notice {
                text: format!(
                    "{name} does not offer {what} {value}; it uses {} instead.",
                    choice["currentValue"].as_str().unwrap_or("its default")
                ),
            });
            continue;
        }
        let params = json!({"sessionId": id, "configId": choice["id"], "value": value});
        match connection
            .request("session/set_config_option", params)
            .await
        {
            Ok(answer) if answer["configOptions"].is_array() => {
                options = answer["configOptions"].clone();
            }
            Ok(_) => {}
            Err(error) => out.push(ChatEvent::Notice {
                text: format!("{name} rejected {what} {value}: {error}."),
            }),
        }
    }
    Ok(Session { id, steering })
}

struct Client {
    profile: &'static Profile,
    connection: Arc<Connection>,
    turn: Arc<Mutex<Turn>>,
    updates: mpsc::UnboundedSender<Update>,
    session: Session,
}

impl Client {
    fn handle(self: &Arc<Self>, command: Command) {
        match command {
            Command::Prompt(text) => {
                let busy = self.turn.lock().expect("turn lock").prompts > 0;
                if busy && self.session.steering {
                    self.steer(text);
                } else {
                    self.prompt(text);
                }
            }
            Command::Abort => self
                .connection
                .notify("session/cancel", json!({"sessionId": self.session.id})),
            Command::Stop => {}
        }
    }

    fn prompt(self: &Arc<Self>, text: String) {
        let mut out = Vec::new();
        self.turn.lock().expect("turn lock").prompt_sent(&mut out);
        emit(&self.updates, out);
        let client = self.clone();
        tokio::spawn(async move {
            let params = json!({
                "sessionId": client.session.id,
                "prompt": [{"type": "text", "text": text}],
            });
            let answer = client.connection.request("session/prompt", params).await;
            let mut out = Vec::new();
            if let Err(error) = &answer {
                out.push(ChatEvent::Notice {
                    text: format!("{} could not answer: {error}", client.profile.name),
                });
            }
            let stop_reason = answer.as_ref().ok().and_then(|a| a["stopReason"].as_str());
            client
                .turn
                .lock()
                .expect("turn lock")
                .prompt_ended(stop_reason, &mut out);
            emit(&client.updates, out);
        });
    }

    /// Adds the message to the running turn, or prompts with it when the turn
    /// ended meanwhile.
    fn steer(self: &Arc<Self>, text: String) {
        let client = self.clone();
        tokio::spawn(async move {
            let params = json!({
                "sessionId": client.session.id,
                "prompt": [{"type": "text", "text": text}],
            });
            if client
                .connection
                .request(STEER_METHOD, params)
                .await
                .is_err()
            {
                client.prompt(text);
            }
        });
    }
}

/// Drives an ACP agent until it exits. See [`super::Harness::attach`].
pub fn attach(
    profile: &'static Profile,
    process: Process,
    spec: &LaunchSpec,
    mut commands: mpsc::UnboundedReceiver<Command>,
    updates: mpsc::UnboundedSender<Update>,
) {
    let Process {
        stdin,
        stdout,
        stop,
        exited,
        trace,
    } = process;
    let turn = Arc::new(Mutex::new(Turn::default()));

    let incoming_turn = turn.clone();
    let incoming_updates = updates.clone();
    let (connection, reader) = connect(
        stdin,
        stdout,
        trace,
        move |connection: &Connection, message| {
            let mut out = Vec::new();
            match message {
                Incoming::Notification { method, params } if method == "session/update" => {
                    incoming_turn
                        .lock()
                        .expect("turn lock")
                        .update(&params["update"], &mut out);
                }
                Incoming::Notification { .. } => {}
                Incoming::Request { id, method, params }
                    if method == "session/request_permission" =>
                {
                    let call = &params["toolCall"];
                    incoming_turn
                        .lock()
                        .expect("turn lock")
                        .tool_update(call, true, &mut out);
                    match grant(&params["options"]) {
                        Some(granted) => connection.reply(&id, granted),
                        None => {
                            connection.reply(&id, json!({"outcome": {"outcome": "cancelled"}}));
                            out.push(ChatEvent::Notice {
                            text: format!(
                                "{} asked to use {} without a way to allow it, so it was declined.",
                                profile.name,
                                tool_name(call)
                            ),
                        });
                        }
                    }
                }
                Incoming::Request { id, method, .. } => {
                    connection.refuse(&id, "the factory does not offer this");
                    out.push(ChatEvent::Notice {
                    text: format!(
                        "{} asked for {method}, which the chat cannot answer, so it was declined.",
                        profile.name
                    ),
                });
                }
            }
            emit(&incoming_updates, out);
        },
    );

    let start = Start {
        state_dir: spec.state_dir.to_owned(),
        cwd: spec.cwd.to_owned(),
        model: spec.model.to_owned(),
        reasoning: spec.reasoning.to_owned(),
        resume: match spec.resume {
            Resume::No => None,
            Resume::Recorded { id, .. } => Some(Some(id.to_owned())),
            Resume::Newest => Some(None),
        },
    };
    let session_updates = updates.clone();
    // Why the session never opened, which makes the whole run a failure.
    let not_opened = Arc::new(Mutex::new(None::<String>));
    let opening_failure = not_opened.clone();
    tokio::spawn(async move {
        emit(
            &session_updates,
            vec![ChatEvent::Notice {
                text: format!("{} is opening its session.", profile.name),
            }],
        );
        // A stop ends the session even while it is still opening; messages
        // sent meanwhile wait for it.
        let mut out = Vec::new();
        let mut early = Vec::new();
        let opened = {
            let opening = tokio::time::timeout(
                OPEN_TIMEOUT,
                open_session(&connection, profile, &start, &mut out),
            );
            tokio::pin!(opening);
            loop {
                tokio::select! {
                    opened = &mut opening => break Some(opened.unwrap_or_else(|_| Err(format!(
                        "it did not open one within {} seconds",
                        OPEN_TIMEOUT.as_secs()
                    )))),
                    command = commands.recv() => match command {
                        Some(Command::Stop) | None => break None,
                        Some(command) => {
                            echo(&session_updates, &command);
                            early.push(command);
                        }
                    },
                }
            }
        };
        emit(&session_updates, out);
        match opened {
            Some(Ok(session)) => {
                let _ = session_updates.send(Update::Identity {
                    session_id: session.id.clone(),
                    session_file: (profile.session_file)(&start.state_dir, &start.cwd, &session.id),
                });
                let client = Arc::new(Client {
                    profile,
                    connection: connection.clone(),
                    turn,
                    updates: session_updates,
                    session,
                });
                for command in early {
                    client.handle(command);
                }
                while let Some(command) = commands.recv().await {
                    if matches!(command, Command::Stop) {
                        break;
                    }
                    echo(&client.updates, &command);
                    client.handle(command);
                }
            }
            Some(Err(error)) => {
                *opening_failure.lock().expect("failure lock") =
                    Some(format!("{} did not open a session: {error}", profile.name));
            }
            None => {}
        }
        connection.close();
        let _ = stop.send(());
    });

    tokio::spawn(async move {
        let exit = exited.await.unwrap_or_else(|_| {
            Some(format!(
                "the environment running {} went away",
                profile.name
            ))
        });
        let _ = reader.await;
        let opened = not_opened.lock().expect("failure lock").take();
        let failure = match (opened, exit) {
            (Some(opened), Some(exit)) => Some(format!("{opened}. {exit}")),
            (opened, exit) => opened.or(exit),
        };
        let _ = updates.send(Update::Exited { failure });
    });
}
