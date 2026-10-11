//! Claude Code, driven through its ACP adapter `claude-agent-acp`. The adapter
//! is built on the Claude Agent SDK, which brings the Claude Code binary along.
//! Ended sessions are read from the JSONL transcript Claude Code writes itself
//! into its config directory, which is pointed at the harness state directory.

use super::{
    Block, CatalogSource, ChatItem, Command, Harness, Invocation, LaunchSpec, Process,
    TranscriptReader, Update, acp, clip,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    fs, io,
    path::{Path, PathBuf},
    time::SystemTime,
};
use tokio::sync::mpsc;

pub struct Claude;

static PROFILE: acp::Profile = acp::Profile {
    name: "Claude Code",
    // The microvm is the boundary, so tools run without asking. As root this
    // mode needs `IS_SANDBOX`; without it, permission requests are granted
    // one by one instead.
    unattended_mode: Some("bypassPermissions"),
    session_file,
    session_meta,
};

/// Recent models leave their thinking out of the answer unless asked, so a
/// long think would look like a stall. These settings take precedence over
/// any settings file.
fn session_meta() -> Value {
    serde_json::json!({"claudeCode": {"options": {"settings": {"showThinkingSummaries": true}}}})
}

/// Claude Code names a project's directory after its path, with everything
/// but letters and digits replaced by `-`.
fn session_file(state_dir: &str, cwd: &str, session_id: &str) -> Option<String> {
    let project: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(format!("{state_dir}/projects/{project}/{session_id}.jsonl"))
}

fn invocation(spec: &LaunchSpec) -> Invocation {
    let env = vec![
        // Update checks, telemetry and error reports would only be refused by
        // the proxy, which lets the model API through and nothing else.
        (
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_owned(),
            "1".to_owned(),
        ),
        // Guests run harnesses as root. This tells Claude Code it is in a
        // sandbox, where it allows skipping its permission prompts as root.
        ("IS_SANDBOX".to_owned(), "1".to_owned()),
        ("CLAUDE_CONFIG_DIR".to_owned(), spec.state_dir.to_owned()),
        // Claude Code's model choice offers its own aliases; this adds the
        // catalog's full model id to them, so the session can be set to it.
        (
            "ANTHROPIC_CUSTOM_MODEL_OPTION".to_owned(),
            spec.model.to_owned(),
        ),
    ];
    Invocation {
        program: "claude-agent-acp".into(),
        args: Vec::new(),
        env,
    }
}

// ---- transcript reader ----

/// Milliseconds since the epoch of a UTC time such as `2026-10-10T16:59:33.694Z`.
fn timestamp(text: &str) -> Option<i64> {
    let (date, time) = text.strip_suffix('Z')?.split_once('T')?;
    let numbers = |part: &str, separator| -> Option<Vec<i64>> {
        part.split(separator).map(|n| n.parse().ok()).collect()
    };
    let [year, month, day] = numbers(date, '-')?[..] else {
        return None;
    };
    let (clock, fraction) = time.split_once('.').unwrap_or((time, "0"));
    let [hour, minute, second] = numbers(clock, ':')?[..] else {
        return None;
    };
    let millis: i64 = format!("{fraction:0<3}").get(..3)?.parse().ok()?;
    // Days from the civil date, after Howard Hinnant's algorithm.
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some((((days * 24 + hour) * 60 + minute) * 60 + second) * 1000 + millis)
}

fn block_text(block: &Value) -> Option<String> {
    match block {
        Value::String(text) => Some(text.clone()),
        _ => match block["type"].as_str()? {
            "text" => block["text"].as_str().map(str::to_owned),
            "image" => Some("[image]".to_owned()),
            _ => None,
        },
    }
}

fn text_of(content: &Value) -> String {
    match content {
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(block_text)
            .collect::<Vec<_>>()
            .join("\n"),
        content => block_text(content).unwrap_or_default(),
    }
}

/// The most recently written transcript of any project.
fn newest_transcript(projects: &Path) -> Option<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for project in fs::read_dir(projects).ok()?.flatten() {
        let Ok(files) = fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            if let Ok(modified) = file.metadata().and_then(|meta| meta.modified())
                && newest.as_ref().is_none_or(|(time, _)| modified > *time)
            {
                newest = Some((modified, path));
            }
        }
    }
    newest.map(|(_, path)| path)
}

/// Records form a tree through `parentUuid`. The conversation is the path from
/// the root to the newest record that belongs to it; subagents' records are
/// sidechains.
fn conversation(records: &[Value]) -> Vec<&Value> {
    let by_uuid: HashMap<&str, &Value> = records
        .iter()
        .filter_map(|record| Some((record["uuid"].as_str()?, record)))
        .collect();
    let mut cursor = records
        .iter()
        .rev()
        .find(|record| record["uuid"].is_string() && record["isSidechain"] != true);
    let mut path = Vec::new();
    while let Some(record) = cursor
        && path.len() < records.len()
    {
        path.push(record);
        cursor = record["parentUuid"]
            .as_str()
            .and_then(|id| by_uuid.get(id).copied());
    }
    path.reverse();
    path
}

fn assistant_blocks(content: &Value, tool_names: &mut HashMap<String, String>) -> Vec<Block> {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| match block["type"].as_str()? {
            "text" => Some(Block::Text {
                text: block["text"]
                    .as_str()
                    .filter(|t| !t.trim().is_empty())?
                    .into(),
            }),
            // Thinking may be stored redacted, as an empty text.
            "thinking" => Some(Block::Thinking {
                text: block["thinking"]
                    .as_str()
                    .filter(|t| !t.trim().is_empty())?
                    .into(),
            }),
            "tool_use" | "server_tool_use" => {
                let id = block["id"].as_str()?.to_owned();
                let name = block["name"].as_str()?.to_owned();
                tool_names.insert(id.clone(), name.clone());
                Some(Block::ToolCall {
                    id,
                    name,
                    arguments: block["input"].clone(),
                })
            }
            _ => None,
        })
        .collect()
}

/// Claude Code records an interruption as a user message of its own.
fn is_interruption(text: &str) -> bool {
    text.starts_with("[Request interrupted by user")
}

/// Slash commands and their output are stored as tagged user messages.
fn is_command(text: &str) -> bool {
    text.starts_with("<command-") || text.starts_with("<local-command-")
}

/// The chat marks an interrupted message as aborted, like a live session does.
fn mark_aborted(items: &mut Vec<ChatItem>, ts: i64) {
    match items.last_mut() {
        Some(ChatItem::Assistant { stop_reason, .. }) => *stop_reason = Some("aborted".into()),
        _ => items.push(ChatItem::Assistant {
            blocks: Vec::new(),
            model: None,
            stop_reason: Some("aborted".into()),
            error: None,
            ts,
        }),
    }
}

fn user_items(
    record: &Value,
    ts: i64,
    tool_names: &HashMap<String, String>,
    items: &mut Vec<ChatItem>,
) {
    let content = &record["message"]["content"];
    if record["isCompactSummary"] == true {
        items.push(ChatItem::Notice {
            text: format!("Earlier conversation summarised: {}", text_of(content)),
            ts,
        });
        return;
    }
    let blocks = match content {
        Value::String(_) => std::slice::from_ref(content),
        Value::Array(blocks) => blocks.as_slice(),
        _ => return,
    };
    let mut texts = Vec::new();
    for block in blocks {
        match block {
            block if block["type"] == "tool_result" => {
                let id = block["tool_use_id"].as_str().unwrap_or("").to_owned();
                items.push(ChatItem::ToolResult {
                    tool_name: tool_names.get(&id).cloned().unwrap_or_default(),
                    tool_call_id: id,
                    text: clip(text_of(&block["content"])),
                    is_error: block["is_error"] == true,
                    ts,
                });
            }
            block => texts.extend(block_text(block)),
        }
    }
    let text = texts.join("\n");
    if is_interruption(&text) {
        mark_aborted(items, ts);
    } else if !text.trim().is_empty() && !is_command(&text) {
        items.push(ChatItem::User { text, ts });
    }
}

fn items(records: &[Value]) -> Vec<ChatItem> {
    let mut items = Vec::new();
    let mut tool_names = HashMap::new();
    // One API message is stored as one record per content block.
    let mut message_id: Option<String> = None;
    for record in conversation(records) {
        let ts = record["timestamp"]
            .as_str()
            .and_then(timestamp)
            .unwrap_or(0);
        let message = &record["message"];
        match record["type"].as_str() {
            Some("assistant") => {
                let id = message["id"].as_str();
                let blocks = assistant_blocks(&message["content"], &mut tool_names);
                let stop = message["stop_reason"].as_str().map(str::to_owned);
                if id.is_some()
                    && id == message_id.as_deref()
                    && let Some(ChatItem::Assistant {
                        blocks: so_far,
                        stop_reason,
                        ..
                    }) = items.last_mut()
                {
                    so_far.extend(blocks);
                    if stop.is_some() {
                        *stop_reason = stop;
                    }
                    continue;
                }
                message_id = id.map(str::to_owned);
                let error =
                    (record["isApiErrorMessage"] == true).then(|| text_of(&message["content"]));
                // Claude Code answers slash commands itself; only its error
                // reports among those are worth showing.
                if message["model"] == "<synthetic>" && error.is_none() {
                    continue;
                }
                items.push(ChatItem::Assistant {
                    blocks: if error.is_some() { Vec::new() } else { blocks },
                    model: message["model"].as_str().map(str::to_owned),
                    stop_reason: stop,
                    error,
                    ts,
                });
            }
            Some("user") if record["isMeta"] != true => {
                message_id = None;
                user_items(record, ts, &tool_names, &mut items);
            }
            Some("system") if record["subtype"] == "compact_boundary" => {
                message_id = None;
                items.push(ChatItem::Notice {
                    text: "Earlier conversation summarised.".into(),
                    ts,
                });
            }
            _ => {}
        }
    }
    items
}

impl TranscriptReader for Claude {
    fn read(&self, state_dir: &Path, session_file: Option<&Path>) -> io::Result<Vec<ChatItem>> {
        let newest;
        let file = match session_file {
            Some(file) => file,
            None => match newest_transcript(&state_dir.join("projects")) {
                Some(file) => {
                    newest = file;
                    &newest
                }
                None => return Ok(Vec::new()),
            },
        };
        let records: Vec<Value> = fs::read_to_string(file)?
            .split('\n')
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(items(&records))
    }
}

impl Harness for Claude {
    /// Model and reasoning level are chosen over ACP once the session is open.
    fn invocation(&self, spec: &LaunchSpec) -> Invocation {
        invocation(spec)
    }

    fn attach(
        &self,
        process: Process,
        spec: &LaunchSpec,
        commands: mpsc::UnboundedReceiver<Command>,
        updates: mpsc::UnboundedSender<Update>,
    ) {
        acp::attach(&PROFILE, process, spec, commands, updates);
    }

    fn catalog(&self) -> Option<CatalogSource> {
        Some(CatalogSource {
            provider: "anthropic",
            base_url: "https://api.anthropic.com",
        })
    }
}
