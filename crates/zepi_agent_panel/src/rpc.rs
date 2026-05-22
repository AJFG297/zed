use std::{
    env,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use anyhow::{Context, Result, anyhow};
use async_channel::{Receiver, Sender};
use serde::Serialize;

#[derive(Debug)]
pub enum RpcClientEvent {
    StdoutLine(String),
    StderrLine(String),
    Exited(Option<i32>),
    Failed(String),
}

pub struct RpcClient {
    stdin: mpsc::Sender<String>,
    kill: mpsc::Sender<()>,
}

impl RpcClient {
    pub fn spawn(cwd: Option<PathBuf>) -> Result<(Self, Receiver<RpcClientEvent>)> {
        let command = resolve_zepi_command()?;
        let mut process = Command::new(&command.program);
        process
            .args(&command.args)
            .env("ZEPI_ROOT", &command.zepi_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            process.current_dir(cwd);
        }

        #[cfg(windows)]
        process.creation_flags(0x08000000);

        let mut child = process.spawn().with_context(|| {
            format!(
                "spawning {} {}",
                command.program.display(),
                command.args.join(" ")
            )
        })?;
        let stdout = child.stdout.take().context("taking Zepi RPC stdout")?;
        let stderr = child.stderr.take().context("taking Zepi RPC stderr")?;
        let stdin = child.stdin.take().context("taking Zepi RPC stdin")?;
        let (event_tx, event_rx) = async_channel::unbounded();
        let (stdin_tx, stdin_rx) = mpsc::channel();
        let (kill_tx, kill_rx) = mpsc::channel();

        spawn_stdout_reader(stdout, event_tx.clone());
        spawn_stderr_reader(stderr, event_tx.clone());
        spawn_stdin_writer(stdin, stdin_rx, event_tx.clone());
        spawn_waiter(child, kill_rx, event_tx);

        Ok((
            Self {
                stdin: stdin_tx,
                kill: kill_tx,
            },
            event_rx,
        ))
    }

    pub fn send(&self, command: RpcCommand) -> Result<()> {
        self.stdin
            .send(command.to_json_line()?)
            .map_err(|_| anyhow!("Zepi RPC stdin writer is closed"))
    }

    pub fn send_json_line(&self, line: String) -> Result<()> {
        self.stdin
            .send(format!("{line}\n"))
            .map_err(|_| anyhow!("Zepi RPC stdin writer is closed"))
    }

    pub fn kill(&self) {
        let _ = self.kill.send(());
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        self.kill();
    }
}

fn spawn_stdout_reader(
    stdout: impl std::io::Read + Send + 'static,
    event_tx: Sender<RpcClientEvent>,
) {
    thread::spawn(move || {
        for line in BufReader::new(stdout).split(b'\n') {
            match line {
                Ok(mut bytes) => {
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                    match String::from_utf8(bytes) {
                        Ok(line) => send_event(&event_tx, RpcClientEvent::StdoutLine(line)),
                        Err(err) => send_event(&event_tx, RpcClientEvent::Failed(err.to_string())),
                    }
                }
                Err(err) => {
                    send_event(&event_tx, RpcClientEvent::Failed(err.to_string()));
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_reader(
    stderr: impl std::io::Read + Send + 'static,
    event_tx: Sender<RpcClientEvent>,
) {
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            match line {
                Ok(line) => send_event(&event_tx, RpcClientEvent::StderrLine(line)),
                Err(err) => {
                    send_event(&event_tx, RpcClientEvent::Failed(err.to_string()));
                    break;
                }
            }
        }
    });
}

fn spawn_stdin_writer(
    mut stdin: ChildStdin,
    stdin_rx: mpsc::Receiver<String>,
    event_tx: Sender<RpcClientEvent>,
) {
    thread::spawn(move || {
        for line in stdin_rx {
            if let Err(err) = stdin.write_all(line.as_bytes()).and_then(|_| stdin.flush()) {
                send_event(&event_tx, RpcClientEvent::Failed(err.to_string()));
                break;
            }
        }
    });
}

fn spawn_waiter(mut child: Child, kill_rx: mpsc::Receiver<()>, event_tx: Sender<RpcClientEvent>) {
    thread::spawn(move || {
        loop {
            if kill_rx.try_recv().is_ok() {
                let _ = child.kill();
            }
            match child.try_wait() {
                Ok(Some(status)) => {
                    send_event(&event_tx, RpcClientEvent::Exited(status.code()));
                    break;
                }
                Ok(None) => thread::sleep(Duration::from_millis(50)),
                Err(err) => {
                    send_event(&event_tx, RpcClientEvent::Failed(err.to_string()));
                    break;
                }
            }
        }
    });
}

fn send_event(event_tx: &Sender<RpcClientEvent>, event: RpcClientEvent) {
    let _ = event_tx.try_send(event);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcCommand {
    Prompt { message: String },
    Abort,
    NewSession,
    GetCommands,
}

impl RpcCommand {
    pub fn to_json_line(&self) -> Result<String> {
        let value = match self {
            RpcCommand::Prompt { message } => WireCommand::Prompt {
                command_type: "prompt",
                message,
            },
            RpcCommand::Abort => WireCommand::Simple {
                command_type: "abort",
            },
            RpcCommand::NewSession => WireCommand::Simple {
                command_type: "new_session",
            },
            RpcCommand::GetCommands => WireCommand::Simple {
                command_type: "get_commands",
            },
        };
        Ok(format!("{}\n", serde_json::to_string(&value)?))
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum WireCommand<'a> {
    Prompt {
        #[serde(rename = "type")]
        command_type: &'static str,
        message: &'a str,
    },
    Simple {
        #[serde(rename = "type")]
        command_type: &'static str,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZepiCommand {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub zepi_root: PathBuf,
}

pub fn resolve_zepi_command() -> Result<ZepiCommand> {
    let zepi_root = resolve_zepi_root()?;
    let program = if cfg!(windows) {
        zepi_root.join("scripts").join("bin").join("zepi.cmd")
    } else {
        zepi_root.join("scripts").join("bin").join("zepi")
    };

    if !program.exists() {
        return Err(anyhow!("Zepi launcher not found at {}", program.display()));
    }

    Ok(ZepiCommand {
        program,
        args: vec!["--mode".to_owned(), "rpc".to_owned()],
        zepi_root,
    })
}

fn resolve_zepi_root() -> Result<PathBuf> {
    if let Some(root) = env::var_os("ZEPI_ROOT").map(PathBuf::from) {
        return Ok(root);
    }

    for start in [env::current_exe().ok(), env::current_dir().ok()]
        .into_iter()
        .flatten()
    {
        let start = if start.is_file() {
            start.parent().map(Path::to_path_buf).unwrap_or(start)
        } else {
            start
        };
        for candidate in start.ancestors() {
            if looks_like_zepi_root(candidate) {
                return Ok(candidate.to_path_buf());
            }
        }
    }

    Err(anyhow!(
        "ZEPI_ROOT is not set and no Zepi checkout could be inferred"
    ))
}

fn looks_like_zepi_root(path: &Path) -> bool {
    path.join("scripts")
        .join("bin")
        .join(if cfg!(windows) { "zepi.cmd" } else { "zepi" })
        .exists()
        && path.join("packages").join("zepi-pi-extensions").exists()
        && path.join("pi").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn serializes_prompt_command_as_strict_jsonl() {
        assert_eq!(
            RpcCommand::Prompt {
                message: "/plan hello".to_owned()
            }
            .to_json_line()
            .unwrap(),
            "{\"type\":\"prompt\",\"message\":\"/plan hello\"}\n"
        );
    }

    #[test]
    fn serializes_get_commands_command() {
        assert_eq!(
            RpcCommand::GetCommands.to_json_line().unwrap(),
            "{\"type\":\"get_commands\"}\n"
        );
    }
}
