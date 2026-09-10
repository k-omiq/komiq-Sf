//! The Cloudflare Tunnel connector: `cloudflared tunnel run`, supervised.
//!
//! The server starts `cloudflared` as a child when tunnel setup reaches that step and
//! again at every boot while a tunnel is set up, restarts it with a backoff when it
//! exits, and stops it on "Remove". The binary comes from `CLOUDFLARED_BIN` or `PATH`.
//!
//! **The run token.** The connector authenticates with a tunnel token: whoever holds it
//! can serve traffic for the tunnel's hostname. It must survive restarts (the setup's
//! API token is not kept, so it cannot be fetched again at boot), so it lives in a file
//! next to the database, mode 0600, outside the `instance_setting` table: the database
//! is what gets backed up and replicated, the file is not. It reaches `cloudflared`
//! through the `TUNNEL_TOKEN` environment variable rather than `--token`, so it never
//! shows in a process listing, and every log line we keep is scrubbed of it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::watch;

/// What Settings › Access shows for the connector.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Off,
    Starting,
    Running,
    /// Exited; a restart is scheduled.
    Failed,
    /// No `cloudflared` binary found.
    Missing,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Starting => "starting",
            State::Running => "running",
            State::Failed => "failed",
            State::Missing => "missing",
        }
    }
}

#[derive(Default)]
struct Inner {
    state: Option<State>,
    message: Option<String>,
    stop: Option<watch::Sender<bool>>,
    task: Option<tokio::task::JoinHandle<()>>,
}

pub struct Connector {
    bin: Option<String>,
    inner: Arc<Mutex<Inner>>,
}

/// Lines `cloudflared` prints once an edge connection is up.
const CONNECTED_MARKERS: &[&str] = &["Registered tunnel connection", "Connection registered"];

impl Connector {
    /// `bin`: an explicit path (`CLOUDFLARED_BIN`); `None` searches `PATH`.
    pub fn new(bin: Option<String>) -> Self {
        Self {
            bin,
            inner: Arc::new(Mutex::new(Inner::default())),
        }
    }

    pub fn status(&self) -> (State, Option<String>) {
        let g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        (g.state.unwrap_or(State::Off), g.message.clone())
    }

    fn set(inner: &Mutex<Inner>, state: State, message: Option<String>) {
        let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
        g.state = Some(state);
        g.message = message;
    }

    /// The binary to run, or a message saying how to provide one.
    pub fn resolve_bin(&self) -> Result<PathBuf, String> {
        if let Some(b) = self.bin.as_deref().filter(|b| !b.trim().is_empty()) {
            let p = PathBuf::from(b);
            return if p.is_file() {
                Ok(p)
            } else {
                Err(format!(
                    "CLOUDFLARED_BIN points at {b}, which does not exist. Install cloudflared \
                     or fix the path."
                ))
            };
        }
        let path = std::env::var_os("PATH").unwrap_or_default();
        std::env::split_paths(&path)
            .map(|d| d.join("cloudflared"))
            .find(|p| p.is_file())
            .ok_or_else(|| {
                "cloudflared was not found. Install cloudflared on this machine, or set \
                 CLOUDFLARED_BIN to its path, then retry."
                    .to_string()
            })
    }

    /// Start (or restart) the supervised connector reading its token from
    /// `token_file`. Returns once the child is spawned; [`Connector::wait_running`]
    /// waits for the edge connection.
    pub fn start(&self, token_file: &Path) -> Result<(), String> {
        let bin = match self.resolve_bin() {
            Ok(b) => b,
            Err(msg) => {
                Self::set(&self.inner, State::Missing, Some(msg.clone()));
                return Err(msg);
            }
        };
        self.stop_now();
        let (tx, rx) = watch::channel(false);
        let inner = self.inner.clone();
        let token_file = token_file.to_path_buf();
        Self::set(&self.inner, State::Starting, None);
        let task = tokio::spawn(supervise(bin, token_file, inner, rx));
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        g.stop = Some(tx);
        g.task = Some(task);
        Ok(())
    }

    /// Wait until the connector reports an edge connection, up to `timeout`.
    pub async fn wait_running(&self, timeout: Duration) -> Result<(), String> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (state, message) = self.status();
            match state {
                State::Running => return Ok(()),
                State::Missing | State::Off => {
                    return Err(message.unwrap_or_else(|| "The connector is not running.".into()))
                }
                State::Failed if tokio::time::Instant::now() >= deadline => {
                    return Err(message.unwrap_or_else(|| "The connector exited.".into()))
                }
                _ if tokio::time::Instant::now() >= deadline => {
                    return Err(format!(
                        "The connector did not connect to Cloudflare within {} seconds.{}",
                        timeout.as_secs(),
                        message
                            .map(|m| format!(" Last message: {m}"))
                            .unwrap_or_default()
                    ))
                }
                _ => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
    }

    /// Signal the supervisor to stop without waiting (used before a restart).
    fn stop_now(&self) -> Option<tokio::task::JoinHandle<()>> {
        let mut g = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(tx) = g.stop.take() {
            let _ = tx.send(true);
        }
        g.task.take()
    }

    /// Stop the connector and wait for the child to exit.
    pub async fn stop(&self) {
        if let Some(task) = self.stop_now() {
            let _ = tokio::time::timeout(Duration::from_secs(10), task).await;
        }
        Self::set(&self.inner, State::Off, None);
    }
}

/// Run `cloudflared` until stopped, restarting it with a growing pause after each exit.
async fn supervise(
    bin: PathBuf,
    token_file: PathBuf,
    inner: Arc<Mutex<Inner>>,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = Duration::from_secs(2);
    loop {
        if *stop.borrow() {
            return;
        }
        let token = match std::fs::read_to_string(&token_file) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => {
                Connector::set(
                    &inner,
                    State::Failed,
                    Some("The tunnel token file is missing. Run setup again.".into()),
                );
                return;
            }
        };
        Connector::set(&inner, State::Starting, None);
        let child = tokio::process::Command::new(&bin)
            .args(["tunnel", "--no-autoupdate", "run"])
            .env("TUNNEL_TOKEN", &token)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                Connector::set(
                    &inner,
                    State::Failed,
                    Some(format!("Could not start cloudflared: {e}")),
                );
                if wait_or_stop(&mut stop, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        tracing::info!(pid = child.id(), "cloudflared started");
        // cloudflared logs to stderr; stdout is drained the same way so it never blocks.
        for pipe in [
            child
                .stdout
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
            child
                .stderr
                .take()
                .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        ]
        .into_iter()
        .flatten()
        {
            let inner = inner.clone();
            let token = token.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    // cloudflared does not print the token, but never trust that.
                    let line = line.replace(&token, "<redacted>");
                    tracing::debug!(target: "cloudflared", "{line}");
                    let mut g = inner.lock().unwrap_or_else(|p| p.into_inner());
                    if CONNECTED_MARKERS.iter().any(|m| line.contains(m)) {
                        g.state = Some(State::Running);
                    }
                    g.message = Some(line.chars().take(300).collect());
                }
            });
        }
        let started = tokio::time::Instant::now();
        tokio::select! {
            status = child.wait() => {
                let msg = match status {
                    Ok(s) => format!("cloudflared exited ({s})."),
                    Err(e) => format!("cloudflared stopped: {e}"),
                };
                tracing::warn!("{msg}");
                let last = inner.lock().unwrap_or_else(|p| p.into_inner()).message.clone();
                Connector::set(
                    &inner,
                    State::Failed,
                    Some(match last {
                        Some(l) => format!("{msg} Last message: {l}"),
                        None => msg,
                    }),
                );
            }
            _ = stop.changed() => {
                let _ = child.kill().await;
                tracing::info!("cloudflared stopped");
                return;
            }
        }
        // A connector that ran for a while gets a fresh, short backoff.
        if started.elapsed() > Duration::from_secs(120) {
            backoff = Duration::from_secs(2);
        }
        if wait_or_stop(&mut stop, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// Sleep `d` unless asked to stop first; true means stop.
async fn wait_or_stop(stop: &mut watch::Receiver<bool>, d: Duration) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => *stop.borrow(),
        _ = stop.changed() => true,
    }
}
