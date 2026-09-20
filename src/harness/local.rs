//! Runs a harness as a plain process on this machine. Tests only: the product
//! never starts a harness outside an isolated environment.

use super::{Invocation, Process};
use std::{io, path::Path, process::Stdio, time::Duration};
use tokio::sync::oneshot;

pub fn spawn(invocation: &Invocation, cwd: &Path) -> io::Result<Process> {
    let mut child = tokio::process::Command::new(&invocation.program)
        .args(&invocation.args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    let (stop, stop_rx) = oneshot::channel::<()>();
    let (exited_tx, exited) = oneshot::channel();

    tokio::spawn(async move {
        let stopped = async {
            if stop_rx.await.is_err() {
                std::future::pending::<()>().await;
            }
        };
        let failure = tokio::select! {
            status = child.wait() => match status {
                Ok(status) if status.success() => None,
                Ok(status) => Some(format!("exited with {status}")),
                Err(error) => Some(error.to_string()),
            },
            () = stopped => {
                // The adapter closed stdin; give the process a moment to leave.
                if tokio::time::timeout(Duration::from_secs(3), child.wait()).await.is_err() {
                    let _ = child.kill().await;
                }
                None
            }
        };
        let _ = exited_tx.send(failure);
    });

    Ok(Process {
        stdin: Box::new(stdin),
        stdout: Box::new(stdout),
        stop,
        exited,
    })
}
