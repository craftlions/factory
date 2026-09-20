//! pi adapter. Live control speaks pi's RPC mode (`pi --mode rpc`, JSON lines
//! over stdio, see pi's docs/rpc.md). Ended sessions are read from the JSONL
//! session file pi writes itself (docs/session-format.md).

use super::{
    Block, BlockKind, ChatEvent, ChatItem, Command, Harness, Invocation, LaunchSpec, ModelInfo,
    Process, Query, Resume, TranscriptReader, Update, clip,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs, io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    sync::mpsc,
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct Pi;

fn invocation(args: &[&str]) -> Invocation {
    let mut all = vec!["--mode".to_owned(), "rpc".to_owned()];
    all.extend(args.iter().map(|arg| (*arg).to_owned()));
    Invocation {
        program: "pi".into(),
        args: all,
    }
}

// ---- message conversion, shared by the live stream and the file reader ----

fn text_of(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block["type"].as_str() {
                Some("text") => block["text"].as_str().map(str::to_owned),
                Some("image") => Some("[image]".to_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Converts one pi `AgentMessage`. Roles with nothing to show return `None`.
fn item_from_message(message: &Value) -> Option<ChatItem> {
    let ts = message["timestamp"].as_i64().unwrap_or(0);
    let string = |key: &str| message[key].as_str().map(str::to_owned);
    match message["role"].as_str()? {
        "user" => Some(ChatItem::User {
            text: text_of(&message["content"]),
            ts,
        }),
        "assistant" => {
            let blocks = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|block| match block["type"].as_str()? {
                            "text" => Some(Block::Text {
                                text: block["text"].as_str()?.into(),
                            }),
                            "thinking" => Some(Block::Thinking {
                                text: block["thinking"].as_str()?.into(),
                            }),
                            "toolCall" => Some(Block::ToolCall {
                                id: block["id"].as_str()?.into(),
                                name: block["name"].as_str()?.into(),
                                arguments: block["arguments"].clone(),
                            }),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(ChatItem::Assistant {
                blocks,
                model: string("model"),
                stop_reason: string("stopReason"),
                error: string("errorMessage"),
                ts,
            })
        }
        "toolResult" => Some(ChatItem::ToolResult {
            tool_call_id: string("toolCallId")?,
            tool_name: string("toolName").unwrap_or_default(),
            text: clip(text_of(&message["content"])),
            is_error: message["isError"].as_bool().unwrap_or(false),
            ts,
        }),
        "bashExecution" => Some(ChatItem::ToolResult {
            tool_call_id: String::new(),
            tool_name: format!("bash: {}", message["command"].as_str().unwrap_or("")),
            text: clip(message["output"].as_str().unwrap_or("").to_owned()),
            is_error: message["exitCode"].as_i64().is_some_and(|code| code != 0),
            ts,
        }),
        "custom" if message["display"].as_bool() == Some(true) => Some(ChatItem::Notice {
            text: text_of(&message["content"]),
            ts,
        }),
        "compactionSummary" | "branchSummary" => Some(ChatItem::Notice {
            text: format!(
                "Earlier conversation summarised: {}",
                message["summary"].as_str()?
            ),
            ts,
        }),
        _ => None,
    }
}

// ---- session file reader ----

impl TranscriptReader for Pi {
    /// Entries form a tree; the last entry is the current leaf, and the
    /// conversation is the path from the root to it.
    fn read(&self, state_dir: &Path, session_file: Option<&Path>) -> io::Result<Vec<ChatItem>> {
        let newest;
        let file = match session_file {
            Some(file) => file,
            None => {
                let mut files: Vec<_> = fs::read_dir(state_dir)?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
                    .collect();
                files.sort();
                match files.pop() {
                    Some(file) => {
                        newest = file;
                        &newest
                    }
                    None => return Ok(Vec::new()),
                }
            }
        };

        let raw = fs::read_to_string(file)?;
        let entries: Vec<Value> = raw
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        let by_id: HashMap<&str, &Value> = entries
            .iter()
            .filter_map(|entry| Some((entry["id"].as_str()?, entry)))
            .filter(|(_, entry)| entry["type"] != "session")
            .collect();

        let mut path = Vec::new();
        let mut cursor = entries
            .iter()
            .rev()
            .find(|entry| entry["type"] != "session");
        while let Some(entry) = cursor {
            path.push(entry);
            cursor = entry["parentId"]
                .as_str()
                .and_then(|id| by_id.get(id).copied());
        }
        path.reverse();

        let ts_of = |entry: &Value| entry["message"]["timestamp"].as_i64().unwrap_or(0);
        Ok(path
            .into_iter()
            .filter_map(|entry| match entry["type"].as_str()? {
                "message" => item_from_message(&entry["message"]),
                "compaction" | "branch_summary" => Some(ChatItem::Notice {
                    text: format!(
                        "Earlier conversation summarised: {}",
                        entry["summary"].as_str()?
                    ),
                    ts: ts_of(entry),
                }),
                "custom_message" if entry["display"].as_bool() == Some(true) => {
                    Some(ChatItem::Notice {
                        text: text_of(&entry["content"]),
                        ts: ts_of(entry),
                    })
                }
                _ => None,
            })
            .collect())
    }
}

// ---- live session ----

/// Translates one stdout record. Dialog requests from extensions are declined
/// through `replies`, because the chat UI has no dialogs yet and pi would
/// otherwise wait forever.
fn translate(
    record: &Value,
    expected: &(String, String),
    working: &AtomicBool,
    replies: &mpsc::UnboundedSender<String>,
) -> Vec<ChatEvent> {
    let notice = |text: String| vec![ChatEvent::Notice { text }];
    let result_text = |value: &Value| clip(text_of(&value["content"]));
    let tool_id = || record["toolCallId"].as_str().unwrap_or("").to_owned();

    match record["type"].as_str().unwrap_or("") {
        "agent_start" => {
            working.store(true, Ordering::Relaxed);
            vec![ChatEvent::Working { working: true }]
        }
        "agent_settled" => {
            working.store(false, Ordering::Relaxed);
            vec![ChatEvent::Working { working: false }]
        }
        "message_start" if record["message"]["role"] == "assistant" => {
            vec![ChatEvent::MessageStart]
        }
        "message_end" => item_from_message(&record["message"])
            .map(|item| vec![ChatEvent::MessageEnd { item }])
            .unwrap_or_default(),
        "message_update" => {
            let event = &record["assistantMessageEvent"];
            let index = event["contentIndex"].as_u64().unwrap_or(0) as usize;
            let start = |kind| {
                vec![ChatEvent::BlockStart {
                    index,
                    kind,
                    id: event["id"].as_str().map(str::to_owned),
                    name: event["toolName"].as_str().map(str::to_owned),
                }]
            };
            match event["type"].as_str().unwrap_or("") {
                "text_start" => start(BlockKind::Text),
                "thinking_start" => start(BlockKind::Thinking),
                "toolcall_start" => start(BlockKind::ToolCall),
                "text_delta" | "thinking_delta" | "toolcall_delta" => {
                    match event["delta"].as_str() {
                        Some(delta) if !delta.is_empty() => {
                            vec![ChatEvent::BlockDelta {
                                index,
                                delta: delta.to_owned(),
                            }]
                        }
                        _ => Vec::new(),
                    }
                }
                _ => Vec::new(),
            }
        }
        "tool_execution_start" => vec![ChatEvent::ToolStart {
            id: tool_id(),
            name: record["toolName"].as_str().unwrap_or("").to_owned(),
            arguments: record["args"].clone(),
        }],
        "tool_execution_update" => vec![ChatEvent::ToolUpdate {
            id: tool_id(),
            output: result_text(&record["partialResult"]),
        }],
        "tool_execution_end" => vec![ChatEvent::ToolEnd {
            id: tool_id(),
            output: result_text(&record["result"]),
            is_error: record["isError"].as_bool().unwrap_or(false),
        }],
        "auto_retry_start" => notice(format!(
            "Retrying after an error (attempt {} of {}): {}",
            record["attempt"],
            record["maxAttempts"],
            record["errorMessage"].as_str().unwrap_or("unknown error"),
        )),
        "auto_retry_end" if record["success"] == false => notice(format!(
            "Retries failed: {}",
            record["finalError"].as_str().unwrap_or("unknown error")
        )),
        "compaction_start" => notice("Compacting the conversation…".into()),
        "extension_error" => notice(format!(
            "Extension error in {}: {}",
            record["extensionPath"]
                .as_str()
                .unwrap_or("unknown extension"),
            record["error"].as_str().unwrap_or("unknown error"),
        )),
        "extension_ui_request" => match record["method"].as_str().unwrap_or("") {
            "notify" => notice(record["message"].as_str().unwrap_or("").to_owned()),
            method @ ("select" | "confirm" | "input" | "editor") => {
                let reply = json!({
                    "type": "extension_ui_response", "id": record["id"], "cancelled": true,
                });
                let _ = replies.send(reply.to_string());
                notice(format!(
                    "An extension asked a question ({method}: {}). The chat cannot answer \
                     dialogs yet, so it was declined.",
                    record["title"].as_str().unwrap_or("untitled"),
                ))
            }
            _ => Vec::new(),
        },
        "response" if record["command"] == "get_state" && record["success"] == true => {
            model_mismatch(&record["data"], &expected.0, &expected.1)
                .map(|mismatch| notice(format!("{mismatch}.")))
                .unwrap_or_default()
        }
        "response" if record["success"] == false => notice(format!(
            "pi rejected {}: {}",
            record["command"].as_str().unwrap_or("a command"),
            record["error"].as_str().unwrap_or("unknown error"),
        )),
        _ => Vec::new(),
    }
}

async fn write_line(stdin: &mut (impl AsyncWrite + Unpin), line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

impl Harness for Pi {
    fn invocation(&self, spec: &LaunchSpec) -> Invocation {
        let mut args = vec![
            "--provider",
            spec.provider,
            "--model",
            spec.model,
            "--thinking",
            spec.reasoning,
            "--session-dir",
            spec.state_dir,
        ];
        match spec.resume {
            Resume::No => {}
            Resume::File(file) => args.extend(["--session", file]),
            Resume::Newest => args.push("--continue"),
        }
        invocation(&args)
    }

    fn attach(
        &self,
        process: Process,
        spec: &LaunchSpec,
        mut commands: mpsc::UnboundedReceiver<Command>,
        updates: mpsc::UnboundedSender<Update>,
    ) {
        let Process {
            mut stdin,
            stdout,
            stop,
            exited,
        } = process;
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let working = Arc::new(AtomicBool::new(false));

        // Commands become RPC lines. Stop ends the line stream, which closes
        // stdin so pi can exit by itself, and tells the environment to end it.
        let command_working = working.clone();
        let command_lines = line_tx.clone();
        tokio::spawn(async move {
            while let Some(command) = commands.recv().await {
                let line = match command {
                    Command::Prompt(message) if command_working.load(Ordering::Relaxed) => {
                        json!({"type": "prompt", "message": message, "streamingBehavior": "steer"})
                    }
                    Command::Prompt(message) => json!({"type": "prompt", "message": message}),
                    Command::Abort => json!({"type": "abort"}),
                    Command::Stop => break,
                };
                if command_lines.send(line.to_string()).is_err() {
                    return;
                }
            }
            let _ = command_lines.send(String::new());
            let _ = stop.send(());
        });

        tokio::spawn(async move {
            while let Some(line) = line_rx.recv().await {
                // The empty line is the stop marker; dropping stdin sends EOF.
                if line.is_empty() || write_line(&mut stdin, &line).await.is_err() {
                    break;
                }
            }
        });

        // Asked first: the answer identifies pi's session, and the chat shows a
        // notice if pi chose another model.
        let _ = line_tx.send(json!({"type": "get_state"}).to_string());
        let expected = (spec.provider.to_owned(), spec.model.to_owned());

        let reader_tx = updates.clone();
        let reader = tokio::spawn(async move {
            // `lines` splits on LF only, as pi's framing requires.
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(record) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if record["type"] == "response"
                    && record["command"] == "get_state"
                    && let Some(session_id) = record["data"]["sessionId"].as_str()
                {
                    let _ = reader_tx.send(Update::Identity {
                        session_id: session_id.to_owned(),
                        session_file: record["data"]["sessionFile"].as_str().map(str::to_owned),
                    });
                }
                for event in translate(&record, &expected, &working, &line_tx) {
                    if reader_tx.send(Update::Event(event)).is_err() {
                        return;
                    }
                }
            }
        });

        tokio::spawn(async move {
            let failure = exited
                .await
                .unwrap_or_else(|_| Some("the environment running pi went away".into()));
            let _ = reader.await;
            let _ = updates.send(Update::Exited { failure });
        });
    }

    fn catalog_invocation(&self, model: Option<(&str, &str)>) -> Invocation {
        // Offline: a catalog question must not wait on startup network calls.
        match model {
            Some((provider, id)) => invocation(&[
                "--no-session",
                "--offline",
                "--provider",
                provider,
                "--model",
                id,
            ]),
            None => invocation(&["--no-session", "--offline"]),
        }
    }

    fn models(&self, process: Process) -> Query<'static, Vec<ModelInfo>> {
        Box::pin(async move {
            let data = query(process, &["get_available_models"]).await?;
            Ok(data[0]["models"]
                .as_array()
                .map(|models| {
                    models
                        .iter()
                        .filter_map(|model| {
                            let id = model["id"].as_str()?;
                            Some(ModelInfo {
                                provider: model["provider"].as_str()?.to_owned(),
                                id: id.to_owned(),
                                name: model["name"].as_str().unwrap_or(id).to_owned(),
                                context_window: model["contextWindow"].as_u64(),
                                reasoning: model["reasoning"].as_bool().unwrap_or(false),
                                base_url: model["baseUrl"].as_str().map(str::to_owned),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn reasoning_levels(
        &self,
        process: Process,
        provider: String,
        model: String,
    ) -> Query<'static, Vec<String>> {
        Box::pin(async move {
            let data = query(process, &["get_state", "get_available_thinking_levels"]).await?;
            if let Some(mismatch) = model_mismatch(&data[0], &provider, &model) {
                return Err(io::Error::new(io::ErrorKind::NotFound, mismatch));
            }
            Ok(data[1]["levels"]
                .as_array()
                .map(|levels| {
                    levels
                        .iter()
                        .filter_map(|l| l.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default())
        })
    }
}

// ---- catalog queries ----

/// Sends the requests to a throwaway pi and returns each response's `data` in
/// the same order, then has the environment end the process.
async fn query(process: Process, requests: &[&str]) -> io::Result<Vec<Value>> {
    let Process {
        mut stdin,
        stdout,
        stop,
        exited,
    } = process;
    let exchange = async {
        for (id, request) in requests.iter().enumerate() {
            write_line(
                &mut stdin,
                &json!({"id": id.to_string(), "type": request}).to_string(),
            )
            .await?;
        }
        let mut answers = vec![None; requests.len()];
        let mut lines = BufReader::new(stdout).lines();
        while answers.iter().any(Option::is_none) {
            let Some(line) = lines.next_line().await? else {
                return Err(io::Error::other("pi exited before answering"));
            };
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let slot = record["id"]
                .as_str()
                .and_then(|id| id.parse::<usize>().ok());
            let (Some(slot), true) = (slot, record["type"] == "response") else {
                continue;
            };
            if record["success"] != true {
                return Err(io::Error::other(
                    record["error"]
                        .as_str()
                        .unwrap_or("pi reported an error")
                        .to_owned(),
                ));
            }
            if let Some(answer) = answers.get_mut(slot) {
                *answer = Some(record["data"].clone());
            }
        }
        Ok(answers.into_iter().flatten().collect())
    };
    let result = tokio::time::timeout(QUERY_TIMEOUT, exchange)
        .await
        .unwrap_or_else(|_| Err(io::Error::other("pi did not answer in time")));
    let _ = stop.send(());
    let _ = exited.await;
    result
}

/// `--model` is a pattern: pi may resolve it to a different model than the one
/// asked for, so the active model is compared with the request. An id pi does
/// not know is not a mismatch; pi passes it to the provider as a custom model.
fn model_mismatch(state: &Value, provider: &str, model: &str) -> Option<String> {
    let active = &state["model"];
    let matches = active["provider"] == provider && active["id"] == model;
    (!matches).then(|| {
        format!(
            "pi does not offer {provider}/{model}; it selected {}/{} instead",
            active["provider"].as_str().unwrap_or("no provider"),
            active["id"].as_str().unwrap_or("no model"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::super::local;
    use super::*;
    use std::env;

    struct Session {
        commands: mpsc::UnboundedSender<Command>,
        updates: mpsc::UnboundedReceiver<Update>,
    }

    fn start(workspace: &Path, spec: &LaunchSpec) -> Session {
        let process = local::spawn(&Pi.invocation(spec), workspace).unwrap();
        let (commands, command_rx) = mpsc::unbounded_channel();
        let (update_tx, updates) = mpsc::unbounded_channel();
        Pi.attach(process, spec, command_rx, update_tx);
        Session { commands, updates }
    }

    async fn identity(session: &mut Session) -> (String, Option<String>) {
        loop {
            match session
                .updates
                .recv()
                .await
                .expect("pi exited before identifying itself")
            {
                Update::Identity {
                    session_id,
                    session_file,
                } => return (session_id, session_file),
                _ => continue,
            }
        }
    }

    async fn stop(mut session: Session) {
        session
            .commands
            .send(Command::Stop)
            .expect("adapter is alive");
        while let Some(update) = session.updates.recv().await {
            if matches!(update, Update::Exited { .. }) {
                return;
            }
        }
    }

    /// Needs pi installed and configured; sends no prompt, so it costs nothing.
    /// Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn resume_reopens_the_recorded_session_file() {
        let root = env::temp_dir().join(format!("factory-pi-test-{}", std::process::id()));
        let (workspace, state_dir) = (root.join("workspace"), root.join("harness"));
        fs::create_dir_all(&workspace).unwrap();
        fs::create_dir_all(&state_dir).unwrap();
        let state = state_dir.to_str().unwrap();
        let spec = |resume| LaunchSpec {
            state_dir: state,
            provider: "openrouter",
            model: "z-ai/glm-5.3-flash",
            reasoning: "low",
            resume,
        };

        let mut first = start(&workspace, &spec(Resume::No));
        let (first_id, first_file) = identity(&mut first).await;
        stop(first).await;
        let file = first_file.expect("pi reports its session file");

        // pi writes the file only once the conversation has a message, so one
        // is written here in pi's documented format instead of paying for a prompt.
        let header = json!({"type": "session", "version": 3, "id": first_id,
            "timestamp": "2026-01-01T00:00:00.000Z", "cwd": workspace});
        let message = json!({"type": "message", "id": "a1b2c3d4", "parentId": null,
            "timestamp": "2026-01-01T00:00:01.000Z",
            "message": {"role": "user", "content": "hello", "timestamp": 1}});
        fs::write(&file, format!("{header}\n{message}\n")).unwrap();
        // A decoy that sorts later would win a "newest file" lookup.
        fs::write(
            state_dir.join("9999-decoy.jsonl"),
            "{\"type\":\"session\",\"id\":\"decoy\"}\n",
        )
        .unwrap();

        let mut second = start(&workspace, &spec(Resume::File(&file)));
        let (second_id, second_file) = identity(&mut second).await;
        stop(second).await;
        assert_eq!(second_id, first_id);
        assert_eq!(second_file.as_deref(), Some(file.as_str()));
        let items = Pi.read(&state_dir, Some(Path::new(&file))).unwrap();
        assert!(matches!(items.as_slice(), [ChatItem::User { text, .. }] if text == "hello"));

        fs::remove_dir_all(&root).unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn catalog_lists_models_and_levels() {
        let cwd = env::temp_dir();
        let models = Pi
            .models(local::spawn(&Pi.catalog_invocation(None), &cwd).unwrap())
            .await
            .unwrap();
        let model = models
            .iter()
            .find(|m| m.base_url.is_some())
            .expect("a model with a base url");
        let process = local::spawn(
            &Pi.catalog_invocation(Some((&model.provider, &model.id))),
            &cwd,
        )
        .unwrap();
        let levels = Pi
            .reasoning_levels(process, model.provider.clone(), model.id.clone())
            .await
            .unwrap();
        assert!(!levels.is_empty());
    }
}
