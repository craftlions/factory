//! pi adapter. Live control uses pi's RPC mode (`pi --mode rpc`, JSON lines
//! over stdio, see pi's docs/rpc.md). Ended sessions are read from the JSONL
//! session file pi writes itself (docs/session-format.md).

use super::{
    Block, BlockKind, ChatEvent, ChatItem, Command, Harness, LaunchSpec, ModelInfo, Resume,
    Running, TranscriptReader, Update, clip,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    env, fs, io,
    path::Path,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin},
    sync::{mpsc, oneshot},
};

const QUERY_TIMEOUT: Duration = Duration::from_secs(30);
const STOP_GRACE: Duration = Duration::from_secs(3);
const STDERR_TAIL: usize = 20;

pub struct Pi;

fn binary() -> std::ffi::OsString {
    env::var_os("FACTORY_PI_BIN").unwrap_or_else(|| "pi".into())
}

fn command(args: &[&str], cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(binary());
    cmd.arg("--mode")
        .arg("rpc")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

fn describe_spawn_error(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::NotFound {
        io::Error::new(
            io::ErrorKind::NotFound,
            "pi was not found. Install it or set FACTORY_PI_BIN to its path.",
        )
    } else {
        error
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

async fn write_line(stdin: &mut ChildStdin, line: &str) -> io::Result<()> {
    stdin.write_all(line.as_bytes()).await?;
    stdin.write_all(b"\n").await?;
    stdin.flush().await
}

impl Harness for Pi {
    fn launch(&self, spec: &LaunchSpec) -> io::Result<Running> {
        let state_dir = spec.state_dir.to_string_lossy();
        let mut args = vec![
            "--provider",
            spec.provider,
            "--model",
            spec.model,
            "--thinking",
            spec.reasoning,
            "--session-dir",
            &state_dir,
        ];
        let resume_file;
        match spec.resume {
            Resume::No => {}
            Resume::File(file) => {
                resume_file = file.to_string_lossy();
                args.extend(["--session", &resume_file]);
            }
            Resume::Newest => args.push("--continue"),
        }
        let mut child = command(&args, spec.workspace)
            .stderr(Stdio::piped())
            .spawn()
            .map_err(describe_spawn_error)?;

        let mut stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (commands, mut command_rx) = mpsc::unbounded_channel::<Command>();
        let (update_tx, updates) = mpsc::unbounded_channel::<Update>();
        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let working = Arc::new(AtomicBool::new(false));
        let stderr_tail = Arc::new(Mutex::new(Vec::<String>::new()));

        // Commands become RPC lines. Stop closes stdin, which makes pi exit.
        let command_working = working.clone();
        let command_lines = line_tx.clone();
        tokio::spawn(async move {
            while let Some(command) = command_rx.recv().await {
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
            let _ = stop_tx.send(());
        });

        let (close_tx, mut close_rx) = oneshot::channel::<()>();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    line = line_rx.recv() => match line {
                        Some(line) => if write_line(&mut stdin, &line).await.is_err() { break },
                        None => break,
                    },
                    _ = &mut close_rx => break,
                }
            }
            // Dropping stdin here sends EOF.
        });

        // Asked first: the answer identifies pi's session, and the chat shows a
        // notice if pi chose another model.
        let _ = line_tx.send(json!({"type": "get_state"}).to_string());
        let expected = (spec.provider.to_owned(), spec.model.to_owned());

        let reader_tx = update_tx.clone();
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

        let tail = stderr_tail.clone();
        let stderr_reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut tail = tail.lock().expect("stderr tail lock");
                if tail.len() == STDERR_TAIL {
                    tail.remove(0);
                }
                tail.push(line);
            }
        });

        tokio::spawn(async move {
            let (status, stopped) = tokio::select! {
                status = child.wait() => (status, false),
                _ = stop_rx => (stop(&mut child, close_tx).await, true),
            };
            let _ = reader.await;
            let _ = stderr_reader.await;
            let failure = match status {
                _ if stopped => None,
                Ok(status) if status.success() => None,
                Ok(status) => {
                    let tail = stderr_tail.lock().expect("stderr tail lock").join("\n");
                    Some(format!("pi exited with {status}. {tail}").trim().to_owned())
                }
                Err(error) => Some(format!("waiting for pi failed: {error}")),
            };
            let _ = update_tx.send(Update::Exited { failure });
        });

        Ok(Running { commands, updates })
    }
}

/// Closes stdin so pi can exit on its own, then kills it after a grace period.
async fn stop(
    child: &mut Child,
    close: oneshot::Sender<()>,
) -> io::Result<std::process::ExitStatus> {
    let _ = close.send(());
    match tokio::time::timeout(STOP_GRACE, child.wait()).await {
        Ok(status) => status,
        Err(_) => {
            child.start_kill()?;
            child.wait().await
        }
    }
}

// ---- catalog queries ----

/// Starts a throwaway pi, sends the requests, and returns each response's
/// `data` in the same order.
async fn query(args: &[&str], requests: &[&str]) -> io::Result<Vec<Value>> {
    let mut cmd = command(args, &env::temp_dir());
    cmd.arg("--no-session").stderr(Stdio::null());
    let mut child = cmd.spawn().map_err(describe_spawn_error)?;
    let mut stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");

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
    let _ = child.kill().await;
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

pub async fn models() -> io::Result<Vec<ModelInfo>> {
    let data = query(&[], &["get_available_models"]).await?;
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
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

pub async fn reasoning_levels(provider: &str, model: &str) -> io::Result<Vec<String>> {
    let data = query(
        &["--provider", provider, "--model", model],
        &["get_state", "get_available_thinking_levels"],
    )
    .await?;
    if let Some(mismatch) = model_mismatch(&data[0], provider, model) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn identity(running: &mut Running) -> (String, Option<String>) {
        loop {
            match running
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

    async fn stop(mut running: Running) {
        running
            .commands
            .send(Command::Stop)
            .expect("adapter is alive");
        while let Some(update) = running.updates.recv().await {
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
        let spec = |resume| LaunchSpec {
            workspace: &workspace,
            state_dir: &state_dir,
            provider: "openrouter",
            model: "z-ai/glm-5.3-flash",
            reasoning: "low",
            resume,
        };

        let mut first = Pi.launch(&spec(Resume::No)).unwrap();
        let (first_id, first_file) = identity(&mut first).await;
        stop(first).await;
        let file = std::path::PathBuf::from(first_file.expect("pi reports its session file"));
        assert!(
            file.starts_with(state_dir.canonicalize().unwrap()) || file.starts_with(&state_dir)
        );

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

        let mut second = Pi.launch(&spec(Resume::File(&file))).unwrap();
        let (second_id, second_file) = identity(&mut second).await;
        stop(second).await;
        assert_eq!(second_id, first_id);
        assert_eq!(second_file.as_deref(), file.to_str());
        let items = Pi.read(&state_dir, Some(&file)).unwrap();
        assert!(matches!(items.as_slice(), [ChatItem::User { text, .. }] if text == "hello"));

        fs::remove_dir_all(&root).unwrap();
    }
}
