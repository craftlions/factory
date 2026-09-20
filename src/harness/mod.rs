//! Harness-agnostic chat model and the traits each harness adapter implements.
//!
//! The UI only ever sees [`ChatItem`] and [`ChatEvent`]. An adapter translates
//! its harness's wire protocol into those while a session runs, and parses the
//! harness's own session file into the same items once it has ended.

#[cfg(test)]
pub mod local;
pub mod pi;

use serde::Serialize;
use serde_json::Value;
use std::{future::Future, io, path::Path, pin::Pin};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};

/// Tool output kept per result; harness session files keep the full text.
const MAX_TOOL_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
    },
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
    },
}

/// One finished entry of a conversation. `ts` is Unix milliseconds.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum ChatItem {
    User {
        text: String,
        ts: i64,
    },
    Assistant {
        blocks: Vec<Block>,
        model: Option<String>,
        stop_reason: Option<String>,
        error: Option<String>,
        ts: i64,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        text: String,
        is_error: bool,
        ts: i64,
    },
    Notice {
        text: String,
        ts: i64,
    },
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    Text,
    Thinking,
    ToolCall,
}

/// Live updates. Blocks of the assistant message in flight are addressed by
/// `index`; `MessageEnd` carries the authoritative finished item.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    MessageStart,
    BlockStart {
        index: usize,
        kind: BlockKind,
        id: Option<String>,
        name: Option<String>,
    },
    BlockDelta {
        index: usize,
        delta: String,
    },
    MessageEnd {
        item: ChatItem,
    },
    ToolStart {
        id: String,
        name: String,
        arguments: Value,
    },
    ToolUpdate {
        id: String,
        output: String,
    },
    ToolEnd {
        id: String,
        output: String,
        is_error: bool,
    },
    Working {
        working: bool,
    },
    Notice {
        text: String,
    },
}

#[derive(Debug)]
pub enum Command {
    /// A user message. The adapter queues it if the harness is mid-run.
    Prompt(String),
    /// Interrupt the current run; the session stays usable.
    Abort,
    /// End the session and the harness process.
    Stop,
}

#[derive(Debug)]
pub enum Update {
    Event(ChatEvent),
    /// The harness's own id and session file for this session.
    Identity {
        session_id: String,
        session_file: Option<String>,
    },
    /// The harness process is gone. Always the last update.
    Exited {
        failure: Option<String>,
    },
}

/// How to start a harness. Paths are as the harness sees them, which inside a
/// microvm are guest paths.
pub struct LaunchSpec<'a> {
    /// Directory the harness keeps its own session file in.
    pub state_dir: &'a str,
    pub provider: &'a str,
    pub model: &'a str,
    pub reasoning: &'a str,
    pub resume: Resume<'a>,
}

/// Whether to continue a conversation the harness has already stored.
#[derive(Clone, Copy, Debug)]
pub enum Resume<'a> {
    No,
    /// The recorded session file.
    File(&'a str),
    /// The newest session in `state_dir`, for rows without a recorded file.
    Newest,
}

/// The command line that starts a harness, for whatever environment runs it.
#[derive(Clone, Debug)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
}

/// A harness process, wherever it runs. The environment that started it owns
/// its lifetime; an adapter only speaks the harness's protocol over the pipes.
pub struct Process {
    pub stdin: Box<dyn AsyncWrite + Send + Unpin>,
    pub stdout: Box<dyn AsyncRead + Send + Unpin>,
    /// Asks the environment to end the process. Dropping it unsent is not a stop.
    pub stop: oneshot::Sender<()>,
    /// Resolves once the process is gone, with the reason if it failed.
    pub exited: oneshot::Receiver<Option<String>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    pub context_window: Option<u64>,
    pub reasoning: bool,
    /// Where the provider's API lives: the host a session of this model may reach.
    pub base_url: Option<String>,
}

pub type Query<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

/// Reads a harness's own session file back into chat items.
pub trait TranscriptReader: Send + Sync {
    /// Reads `session_file` when it is known, else the newest session in `state_dir`.
    fn read(&self, state_dir: &Path, session_file: Option<&Path>) -> io::Result<Vec<ChatItem>>;
}

/// Speaks one kind of harness's protocol. It never starts a process itself:
/// an isolated environment does that from the `Invocation` and hands back the
/// `Process`.
pub trait Harness: TranscriptReader {
    fn invocation(&self, spec: &LaunchSpec) -> Invocation;

    /// Drives `process` until it exits: commands go in, updates come out, and
    /// `Update::Exited` is always the last one.
    fn attach(
        &self,
        process: Process,
        spec: &LaunchSpec,
        commands: mpsc::UnboundedReceiver<Command>,
        updates: mpsc::UnboundedSender<Update>,
    );

    /// A throwaway invocation for catalog questions. With a model, the harness
    /// is started on that model so it can be asked about it.
    fn catalog_invocation(&self, model: Option<(&str, &str)>) -> Invocation;
    /// Models the harness can use right now, asked from the harness itself.
    fn models(&self, process: Process) -> Query<'static, Vec<ModelInfo>>;
    /// Reasoning levels the harness reports for the model `process` was started on.
    fn reasoning_levels(
        &self,
        process: Process,
        provider: String,
        model: String,
    ) -> Query<'static, Vec<String>>;
}

pub fn by_id(id: &str) -> Option<&'static dyn Harness> {
    match id {
        "pi" => Some(&pi::Pi),
        _ => None,
    }
}

fn clip(mut text: String) -> String {
    if text.len() > MAX_TOOL_OUTPUT {
        let mut cut = MAX_TOOL_OUTPUT;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… output clipped");
    }
    text
}
