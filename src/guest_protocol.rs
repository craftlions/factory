//! What the factory and the guest agent say to each other. Shared by both
//! binaries, so it only uses serde.
//!
//! The guest opens every connection, to the host's vsock ports below.
//! Firecracker delivers each as a connection to the unix socket
//! `<vsock path>_<port>` on the host.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// JSON lines: first `Spec` from the host, then `GuestEvent`s from the guest
/// and at most one `HostMessage` from the host.
pub const CONTROL_PORT: u32 = 1024;
/// Raw bytes: the harness's stdin and stdout.
pub const STDIO_PORT: u32 = 1025;
/// One connection per outbound TCP connection of the guest; carries an HTTP
/// proxy conversation with the factory.
pub const PROXY_PORT: u32 = 1080;

/// Everything the guest needs to run one harness.
#[derive(Debug, Serialize, Deserialize)]
pub struct Spec {
    /// Written before anything runs, such as credentials and tool configuration.
    pub files: Vec<GuestFile>,
    pub env: BTreeMap<String, String>,
    /// Runs to completion first and must succeed. Empty means no setup.
    pub setup: Vec<String>,
    /// The harness: program followed by its arguments.
    pub harness: Vec<String>,
    /// Working directory for setup and harness; created if missing.
    pub cwd: String,
    /// Local TCP port the guest agent forwards to the factory's proxy.
    pub proxy_port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GuestFile {
    pub path: String,
    pub mode: u32,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Setup,
    Harness,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GuestEvent {
    Phase {
        phase: Phase,
    },
    /// A line of setup output, or of the harness's stderr.
    Log {
        phase: Phase,
        line: String,
    },
    /// A phase's process ended. After `Harness`, or a failed `Setup`, the guest
    /// shuts down.
    Exit {
        phase: Phase,
        code: Option<i32>,
    },
    /// The agent itself could not continue.
    Error {
        message: String,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HostMessage {
    /// End the harness and shut down.
    Stop,
}
