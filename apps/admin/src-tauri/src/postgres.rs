//! Private bundled PostgreSQL. All paths are explicit; no system database or PATH probe.
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, NaiveDateTime, Utc};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};

use crate::logbuf::LogBuf;
#[path = "postgres/paths.rs"]
mod paths;
pub use paths::private_dir;
#[path = "postgres/process.rs"]
pub(crate) mod process;

pub const VERSION: &str = "18.6";
/// The major version of the bundled runtime. A cluster of another major version cannot
/// be started by it; this phase has no upgrade step (see [`check_major`]).
pub const MAJOR: &str = "18";
const TOOLS: &[&str] = &[
  "postgres",
  "initdb",
  "pg_ctl",
  "psql",
  "pg_dump",
  "pg_restore",
];
const READY: Duration = Duration::from_secs(90);

pub fn target_slug() -> String {
  format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

pub fn resolve(resource: &Path) -> PathBuf {
  if let Some(path) = std::env::var_os("KOMIKA_POSTGRES_RUNTIME").filter(|p| !p.is_empty()) {
    return PathBuf::from(path);
  }
  resource.join("postgres").join(target_slug()).join(MAJOR)
}

/// `<server data>/postgres/<major>`: cluster, credentials and the runtime check stamp.
pub fn cluster_dir(server_data: &Path) -> PathBuf {
  server_data.join("postgres").join(MAJOR)
}

fn wrong_major(data: &Path, found: &str, major: &str) -> String {
  format!(
    "The database folder {} was made by PostgreSQL {found}; this version of the app runs \
     PostgreSQL {major} and cannot open or convert it. The data is kept and nothing was \
     changed; no new empty database was made. A newer version of the app with a migration \
     step is needed.",
    data.display()
  )
}

/// Refuse to go on when a cluster of another major version lies in `postgres_dir`
/// (`<server data>/postgres`), whatever the name of its folder. Without this a runtime of
/// a later major would create a new empty cluster in its own folder beside the owner's
/// data and start with an empty library. Nothing is created, changed or removed here.
pub fn check_major(postgres_dir: &Path, major: &str) -> Result<(), String> {
  let Ok(entries) = fs::read_dir(postgres_dir) else {
    return Ok(()); // no database folder yet: a new install
  };
  let mut folders: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
  folders.sort();
  for folder in folders {
    let data = folder.join("data");
    if let Ok(found) = fs::read_to_string(data.join("PG_VERSION")) {
      if found.trim() != major {
        return Err(wrong_major(&data, found.trim(), major));
      }
    }
  }
  Ok(())
}

fn io(error: std::io::Error) -> String {
  error.to_string()
}

/// How long the rename of a new cluster folder waits for other programs to let its
/// files go (Windows).
const RENAME_WAIT: Duration = Duration::from_secs(10);

/// `fs::rename` of the folder `initdb` just filled. Windows refuses to rename a folder
/// while any program has a file in it open ("Access is denied"), and a virus scanner
/// opens every new file for a moment after it was written. So on Windows a refusal for
/// lack of permission is tried again until `wait` is over; then the error is returned as
/// it is. On every other system, and for every other error, there is one try. (`windows`
/// is a parameter so that the rule is tested on every system.)
async fn rename_when_free(
  from: &Path,
  to: &Path,
  windows: bool,
  wait: Duration,
) -> std::io::Result<()> {
  let deadline = Instant::now() + wait;
  // "Access is denied", and 32, "The process cannot access the file because it is being
  // used by another process" (the standard library gives that one no kind of its own).
  let held = |error: &std::io::Error| {
    error.kind() == std::io::ErrorKind::PermissionDenied || error.raw_os_error() == Some(32)
  };
  loop {
    match fs::rename(from, to) {
      Err(error) if windows && held(&error) && Instant::now() < deadline => {
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
      result => return result,
    }
  }
}

/// PostgreSQL refuses to run with administrator rights (Windows) or as root (Unix). It
/// says so before it looks at the data folder, in lines without a `FATAL:` (so
/// `postgres_said` does not find them). The app starts `postgres` with its own rights,
/// so this happens when the app itself has such rights: started with "Run as
/// administrator", or by an account that always has them (User Account Control is off,
/// or it is the built-in Administrator). What the start page then says, or `None` when
/// the output of the failed start is about something else.
fn rights_refusal(postgres_output: &str, data: &Path) -> Option<String> {
  let cause = if postgres_output.contains("administrative permissions is not") {
    "PostgreSQL does not run with administrator rights, and this app has them: it was \
     started with \"Run as administrator\", or this Windows account always has them (User \
     Account Control is off, or it is the built-in Administrator account). Close the app \
     and start it without administrator rights, from an ordinary account."
  } else if postgres_output.contains("\"root\" execution of the PostgreSQL server is not permitted")
  {
    "PostgreSQL does not run as root, and this app was started as root. Close the app and \
     start it as an ordinary user."
  } else {
    return None;
  };
  Some(format!(
    "PostgreSQL did not start. {cause} The library in {} is kept as it is.",
    data.display()
  ))
}

pub fn random_secret() -> Result<String, String> {
  let mut bytes = [0u8; 32];
  rand::rngs::OsRng
    .try_fill_bytes(&mut bytes)
    .map_err(|e| format!("OS randomness unavailable: {e}"))?;
  Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub fn encode(value: &str) -> String {
  value
    .as_bytes()
    .iter()
    .map(|b| match b {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
        (*b as char).to_string()
      }
      _ => format!("%{b:02X}"),
    })
    .collect()
}

fn private_new(path: &Path) -> Result<File, String> {
  let mut opts = OpenOptions::new();
  opts.write(true).create_new(true);
  #[cfg(unix)]
  {
    use std::os::unix::fs::OpenOptionsExt;
    opts.mode(0o600);
  }
  opts.open(path).map_err(io)
}

/// Held until all three stages stop. The lock file is never unlinked (inode races).
pub struct Ownership {
  _file: File,
}
impl Ownership {
  pub fn acquire(root: &Path) -> Result<Self, String> {
    use std::io::{Seek, SeekFrom};
    // Owner warnings of this folder are raised by the supervisor (src/local.rs).
    private_dir(root)?;
    let path = root.join("desktop.lock");
    let owner = root.join("desktop.owner");
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
      use std::os::windows::fs::OpenOptionsExt;
      options.share_mode(0);
    }
    let held = || {
      format!(
        "Another Komiq Server instance owns {} ({}). Close that instance, then Retry.",
        root.display(),
        fs::read_to_string(&owner)
          .unwrap_or_else(|_| "owner unavailable".into())
          .trim()
      )
    };
    let mut file = options
      .open(&path)
      .map_err(|e| format!("{} Lock: {e}", held()))?;
    #[cfg(unix)]
    {
      use std::os::fd::AsRawFd;
      // SAFETY: flock borrows a live descriptor; closing it releases the OS lock.
      if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(held());
      }
    }
    file.seek(SeekFrom::Start(0)).map_err(io)?;
    file.set_len(0).map_err(io)?;
    writeln!(file, "pid {}", std::process::id()).map_err(io)?;
    fs::write(owner, format!("pid {}", std::process::id())).map_err(io)?;
    Ok(Self { _file: file })
  }
}

#[derive(Deserialize, Serialize, Clone)]
pub struct Credentials {
  supervisor: String,
  application: String,
}
impl Credentials {
  pub fn load(root: &Path, existing_data: bool) -> Result<Self, String> {
    let path = root.join("credentials");
    if path.exists() {
      let meta = fs::symlink_metadata(&path).map_err(io)?;
      if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(format!(
          "Credentials must be a regular private file: {}",
          path.display()
        ));
      }
      #[cfg(unix)]
      {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o777 != 0o600 {
          return Err(format!(
            "Credentials permissions must be 600: {}. Fix permissions, then Retry.",
            path.display()
          ));
        }
      }
      let value: Self = serde_json::from_slice(&fs::read(&path).map_err(io)?).map_err(|_| format!("Invalid credentials file: {}. Restore the original credentials from this install's backup, then Retry.", path.display()))?;
      if ![&value.supervisor, &value.application].iter().all(|p| {
        p.len() >= 32
          && p
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_".contains(&c))
      }) {
        return Err("Invalid database credentials; restore this install's original credentials file, then Retry.".into());
      }
      return Ok(value);
    }
    if existing_data {
      return Err(format!("Database exists but credentials are missing: {}. Restore this install's original credentials file; a new password cannot unlock it. Then Retry.", path.display()));
    }
    let value = Self {
      supervisor: random_secret()?,
      application: random_secret()?,
    };
    let tmp = root.join(format!("credentials-{}.tmp", random_secret()?));
    let result = (|| {
      let mut file = private_new(&tmp)?;
      file
        .write_all(&serde_json::to_vec(&value).map_err(|e| e.to_string())?)
        .map_err(io)?;
      file.sync_all().map_err(io)?;
      fs::rename(&tmp, &path).map_err(io)
    })();
    let _ = fs::remove_file(tmp);
    result?;
    Ok(value)
  }
  fn mask(&self, logs: &[Arc<LogBuf>]) {
    for log in logs {
      log.add_secret(&self.supervisor);
      log.add_secret(&self.application);
    }
  }
  pub fn url(&self, port: u16) -> String {
    format!(
      "postgres://komika_app:{}@127.0.0.1:{port}/komika",
      encode(&self.application)
    )
  }
}

#[derive(Deserialize)]
struct Manifest {
  version: String,
  target: String,
  required: Vec<String>,
  files: BTreeMap<String, String>,
}

fn file_hash(path: &Path) -> Result<String, String> {
  let mut stream = File::open(path).map_err(io)?;
  let mut digest = Sha256::new();
  std::io::copy(&mut stream, &mut digest).map_err(io)?;
  Ok(format!("{:x}", digest.finalize()))
}

#[derive(Clone)]
pub struct Runtime {
  pub root: PathBuf,
}
impl Runtime {
  pub fn bin(&self, tool: &str) -> PathBuf {
    self
      .root
      .join("bin")
      .join(format!("{tool}{}", if cfg!(windows) { ".exe" } else { "" }))
  }
  pub fn validate(root: &Path, cache: &Path) -> Result<Self, String> {
    let root = root
      .canonicalize()
      .map_err(|e| format!("Bundled PostgreSQL not found at {}: {e}", root.display()))?;
    let bytes = fs::read(root.join("manifest.json")).map_err(io)?;
    let manifest: Manifest =
      serde_json::from_slice(&bytes).map_err(|e| format!("Invalid PostgreSQL manifest: {e}"))?;
    if manifest.version != VERSION || manifest.target != target_slug() {
      return Err(format!(
        "PostgreSQL manifest must be {VERSION} for {}",
        target_slug()
      ));
    }
    for name in manifest.files.keys().chain(manifest.required.iter()) {
      let relative = Path::new(name);
      if relative.is_absolute()
        || name.contains('\\')
        || relative
          .components()
          .any(|c| !matches!(c, std::path::Component::Normal(_)))
      {
        return Err("Unsafe PostgreSQL manifest path".into());
      }
      let path = root
        .join(relative)
        .canonicalize()
        .map_err(|_| format!("Missing PostgreSQL resource: {name}"))?;
      if !path.starts_with(&root) || !path.is_file() {
        return Err(format!("Invalid PostgreSQL resource: {name}"));
      }
    }
    for needle in ["pg_trgm.control", "pg_trgm--", "pg_trgm."] {
      if !manifest.files.keys().any(|name| {
        Path::new(name)
          .file_name()
          .is_some_and(|b| b.to_string_lossy().contains(needle))
      }) {
        return Err("Bundled pg_trgm files are missing".into());
      }
    }
    // The tools are started from this path and find their own files from it: never the
    // `\\?\` form that `canonicalize` returns on Windows (see `paths::tool_path`).
    let runtime = Self {
      root: paths::tool_path(root),
    };
    // Path + manifest digest invalidates reinstall/moved runtime caches. A changed
    // executable manifest invalidates the same app-version cache as well.
    let stamp = format!(
      "{}\n{}\n{:x}",
      env!("CARGO_PKG_VERSION"),
      runtime.root.display(),
      Sha256::digest(&bytes)
    );
    if fs::read_to_string(cache).ok().as_deref() != Some(&stamp) {
      for tool in TOOLS {
        let path = runtime.bin(tool);
        let name = path
          .strip_prefix(&runtime.root)
          .map_err(|e| e.to_string())?
          .to_string_lossy()
          .replace('\\', "/");
        if manifest.files.get(&name) != Some(&file_hash(&path)?) {
          return Err(format!(
            "Bundled executable checksum failed: {tool}. Reinstall Komiq Server, then Retry."
          ));
        }
      }
      fs::write(cache, stamp).map_err(io)?;
    } else {
      for tool in TOOLS {
        if !runtime.bin(tool).is_file() {
          return Err(format!("Missing PostgreSQL tool: {tool}"));
        }
      }
    }
    Ok(runtime)
  }
}

pub fn retry_port(attempt: usize, output: &str) -> bool {
  attempt + 1 < 5
    && [
      "Address already in use",
      "address already in use",
      "Only one usage of each socket address",
    ]
    .iter()
    .any(|n| output.contains(n))
}

#[derive(Default)]
pub struct RestartBudget(VecDeque<Instant>);
impl RestartBudget {
  pub fn record(&mut self, now: Instant) -> Option<Duration> {
    while self
      .0
      .front()
      .is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60))
    {
      self.0.pop_front();
    }
    self.0.push_back(now);
    (self.0.len() <= 3).then(|| Duration::from_secs(1 << self.0.len()))
  }
}

pub struct Database {
  pub runtime: Runtime,
  root: PathBuf,
  pub data: PathBuf,
  paths: paths::ToolPaths,
  credentials: Credentials,
  pub child: Option<Child>,
  pub port: u16,
  pub log: Arc<LogBuf>,
}
impl Database {
  pub async fn prepare(
    runtime: &Path,
    server_data: &Path,
    logs: &[Arc<LogBuf>],
  ) -> Result<Self, String> {
    private_dir(server_data)?;
    // Every path a PostgreSQL tool is given is built from this one.
    let server_data = paths::tool_path(server_data.canonicalize().map_err(io)?);
    // Before a cluster is created or started at this data path: a PostgreSQL that was
    // started on this very path and now runs in another folder (the app data folder was
    // renamed while its database ran) is ended first. See `end_strays`.
    #[cfg(unix)]
    end_strays(
      &cluster_dir(&server_data).join("data"),
      &logs[0],
      STRAY_WAITS,
    )
    .await?;
    // Before anything is created: a cluster of another major version blocks the start.
    check_major(&server_data.join("postgres"), MAJOR)?;
    let root = cluster_dir(&server_data);
    fs::create_dir_all(&root).map_err(io)?;
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(io)?;
    }
    let runtime = Runtime::validate(runtime, &root.join("runtime-verified"))?;
    let data = root.join("data");
    let credentials = Credentials::load(&root, data.exists())?;
    credentials.mask(logs);
    let paths = paths::ToolPaths::new(&server_data, &runtime.root).await?;
    let mut database = Self {
      runtime,
      root,
      data,
      paths,
      credentials,
      child: None,
      port: 0,
      log: logs[0].clone(),
    };
    database.initialize().await?;
    Ok(database)
  }

  fn command(&self, tool: &str) -> Command {
    // Windows: through the ASCII alias when the install path is not ASCII (the tools
    // find their own files from the path they were started with).
    let mut cmd = Command::new(
      Runtime {
        root: self.paths.runtime.clone(),
      }
      .bin(tool),
    );
    // Prevent inherited libpq service/host/auth/loader settings redirecting tools.
    for (key, _) in std::env::vars_os() {
      let upper = key.to_string_lossy().to_ascii_uppercase();
      if upper.starts_with("PG") || upper.starts_with("LD_") || upper.starts_with("DYLD_") {
        cmd.env_remove(key);
      }
    }
    cmd
      .env(
        "PGPASSFILE",
        cluster_dir(&self.paths.server).join("empty.pgpass"),
      )
      .env("PGCONNECT_TIMEOUT", "2")
      .env("PGCLIENTENCODING", "UTF8")
      .env("PGSSLMODE", "disable")
      .env("PGGSSENCMODE", "disable")
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    crate::child::before_spawn(&mut cmd);
    cmd
  }

  async fn run_tool(
    &self,
    tool: &str,
    args: &[String],
    input: Option<&str>,
    password: Option<&str>,
    bound: Duration,
  ) -> Result<String, String> {
    self
      .run_tool_holding(tool, args, input, password, bound, None)
      .await
  }

  /// `held`: a file the tool keeps open as its standard input for as long as it lives
  /// (the dump lock), so a tool that outlives the app still holds it.
  async fn run_tool_holding(
    &self,
    tool: &str,
    args: &[String],
    input: Option<&str>,
    password: Option<&str>,
    bound: Duration,
    held: Option<File>,
  ) -> Result<String, String> {
    let mut cmd = self.command(tool);
    cmd.args(args);
    if let Some(pw) = password {
      cmd.env("PGPASSWORD", pw);
    }
    if input.is_some() {
      cmd.stdin(Stdio::piped());
    } else if let Some(held) = held {
      cmd.stdin(Stdio::from(held));
    }
    let mut child = cmd
      .spawn()
      .map_err(|e| format!("Could not run bundled {tool}: {e}"))?;
    crate::child::after_spawn_tied(&mut child, &format!("The PostgreSQL tool {tool}"), false)
      .await?;
    if let Some(input) = input {
      if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(input.as_bytes()).await.map_err(io)?;
      }
    }
    let out = tokio::time::timeout(bound, child.wait_with_output())
      .await
      .map_err(|_| {
        format!(
          "{tool} exceeded {} seconds; process stopped",
          bound.as_secs()
        )
      })?
      .map_err(io)?;
    let stdout = String::from_utf8_lossy(&out.stdout).replace("\r\n", "\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stderr.lines() {
      self.log.push(line);
    }
    if !out.status.success() {
      for line in stdout.lines() {
        self.log.push(line);
      }
      return Err(format!(
        "Bundled {tool} failed ({}). The Database log has the cause.",
        out.status
      ));
    }
    Ok(stdout.trim().to_string())
  }

  async fn initialize(&mut self) -> Result<(), String> {
    // Only our incomplete init directories, without following symlinks. A completed
    // data directory is never removed, even when PG_VERSION is absent or invalid.
    for entry in fs::read_dir(&self.root).map_err(io)? {
      let entry = entry.map_err(io)?;
      if entry
        .file_name()
        .to_string_lossy()
        .starts_with("data.init-")
        && entry.file_type().map_err(io)?.is_dir()
      {
        fs::remove_dir_all(entry.path()).map_err(io)?;
      }
    }
    if self.data.exists() {
      check_cluster(&self.data)?;
    } else {
      let temporary_name = format!("data.init-{}", random_secret()?);
      let temporary = self.root.join(&temporary_name);
      let pw_name = format!("initdb-{}.pw", random_secret()?);
      let pw = self.root.join(&pw_name);
      let mut password_file = private_new(&pw)?;
      let password_guard = RemoveFile(pw);
      writeln!(password_file, "{}", self.credentials.supervisor).map_err(io)?;
      drop(password_file);
      // RAII removes the password even if the future is cancelled during initdb.
      let args = vec![
        "-D".into(),
        cluster_dir(&self.paths.server)
          .join(&temporary_name)
          .display()
          .to_string(),
        "--encoding=UTF8".into(),
        "--locale-provider=builtin".into(),
        "--builtin-locale=C.UTF-8".into(),
        "--lc-collate=C".into(),
        "--lc-ctype=C".into(),
        "--auth-local=scram-sha-256".into(),
        "--auth-host=scram-sha-256".into(),
        "--username=komika_supervisor".into(),
        format!(
          "--pwfile={}",
          cluster_dir(&self.paths.server).join(pw_name).display()
        ),
      ];
      #[cfg(feature = "desktop-gate")]
      gate_event("initdb", &self.data, serde_json::json!({}));
      let result = self
        .run_tool("initdb", &args, None, None, Duration::from_secs(180))
        .await;
      drop(password_guard);
      let output = result.map_err(|e| {
        format!(
          "{e} Initializing {}. Retry after correcting the cause.",
          self.data.display()
        )
      })?;
      for line in output.lines() {
        self.log.push(line);
      }
      check_cluster(&temporary)?;
      rename_when_free(&temporary, &self.data, cfg!(windows), RENAME_WAIT)
        .await
        .map_err(io)?;
    }
    let pgpass = self.root.join("empty.pgpass");
    if !pgpass.exists() {
      private_new(&pgpass)?;
    }
    Ok(())
  }

  pub fn url(&self) -> String {
    self.credentials.url(self.port)
  }
  pub fn mask_url(&self, log: &LogBuf) {
    log.add_secret(&self.url());
  }

  /// Look at `postmaster.pid` before a start. The file is never removed or changed here.
  ///
  /// * No process has its PID: left over. PostgreSQL clears the file itself.
  /// * The bundled `postgres` of this install on this data directory (by its arguments,
  ///   the lock file and, on Unix, its working folder), left running by an app that was
  ///   force-quit: stopped in the ordinary way, then the start goes on.
  /// * A PostgreSQL main process whose working folder IS this data folder (the app data
  ///   folder was renamed, or the app was moved, replaced or deleted, while its database
  ///   ran): this folder's database. It is asked to stop with `pg_ctl`, then the start
  ///   goes on (see [`Database::stop_adopted`]).
  /// * Anything else (the PID was given to another program, a process that cannot be
  ///   inspected, a PostgreSQL of another data folder): not ours. It is never signaled,
  ///   one line names it, and it is returned so that the start can tell PostgreSQL about
  ///   it and can explain a refusal.
  /// * A file that cannot be read as a lock file (empty, garbled, PID 0 or 1): returned
  ///   too; PostgreSQL is told nothing and decides.
  ///
  /// PostgreSQL's own lock check on the data directory (the lock file's PID, then the
  /// shared memory the file names) is what protects the data, not this function.
  async fn reconcile(&self) -> Result<Option<LockNote>, String> {
    let path = self.data.join("postmaster.pid");
    let pid = match read_lock(&path) {
      LockRead::Absent => return Ok(None),
      LockRead::Unreadable(reason) => {
        self.log.push(&format!(
          "{} cannot be read as a lock file ({reason}). It was not changed or removed; \
           PostgreSQL decides at start.",
          path.display()
        ));
        return Ok(Some(LockNote::Unreadable(reason)));
      }
      LockRead::File(pid) => pid,
    };
    match process::claim(
      &pid,
      &self.runtime.bin("postgres"),
      &self.data,
      process::LockFacts::read(&path),
      process::holder(pid.pid),
      process::identity,
      |pid| process::place(pid, &self.data),
    ) {
      process::Claim::Stale => Ok(None),
      process::Claim::Ours(identity) => {
        self.log.push(&format!(
          "Stopping owned PostgreSQL left by the previous run (pid {})",
          pid.pid
        ));
        self.stop_identified(&identity).await?;
        Ok(None)
      }
      process::Claim::Here(found) => {
        self.log.push(&adopted_line(&path, &found));
        self.stop_adopted(&found).await?;
        Ok(None)
      }
      process::Claim::Foreign(foreign) => {
        self.log.push(&stale_pid_line(&path, &foreign));
        Ok(Some(LockNote::Foreign(foreign)))
      }
    }
  }

  /// Stop this folder's database that an earlier run left (`Claim::Here`). It is proven
  /// again right before the request: the same process (creation time), still in this
  /// data folder, and still the PID in the lock file, which is the PID `pg_ctl` signals.
  /// The request is `pg_ctl stop -m fast -w` of the current runtime (the same major
  /// version). A database that runs this install's own program and does not stop is
  /// ended like [`Database::stop_identified`] does it; any other is left alone and the
  /// owner is told what to do.
  async fn stop_adopted(&self, found: &process::Adopted) -> Result<(), String> {
    self.stop_adopted_within(found, ADOPTED_EXIT_WAIT).await
  }

  /// [`Database::stop_adopted`]; `exit_wait`: how long the process may take to end after
  /// `pg_ctl` has returned (a test gives a shorter time).
  async fn stop_adopted_within(
    &self,
    found: &process::Adopted,
    exit_wait: Duration,
  ) -> Result<(), String> {
    let pid = found.identity.pid;
    let running = || {
      process::identity(pid)
        .map(|now| now.as_ref() == Some(&found.identity))
        .unwrap_or(true)
    };
    if process::identity(pid)?.as_ref() != Some(&found.identity)
      || process::place(pid, &self.data) != process::Place::Here
      || !self.lock_file_names(pid)
    {
      return Err(format!(
        "The earlier database process {pid} changed while it was looked at; nothing was \
         signaled. Retry."
      ));
    }
    // A shutdown ends with a checkpoint, and PostgreSQL writes the control file of that
    // checkpoint by the path it was started with. When that path is not this folder any
    // more and another database lies there, it is told to quit instead.
    let mode = adopted_stop_mode(&found.identity.args, &self.data);
    if mode == StopMode::Immediate {
      self.log.push(&format!(
        "The earlier database process {pid} was started on another path than this data \
         folder has now, and a database lies at that path. It is told to quit without a \
         shutdown checkpoint (pg_ctl stop -m immediate), because that checkpoint would be \
         written into the other database; this folder's database recovers what was \
         committed at its next start."
      ));
    }
    let asked = self.stop_with(mode).await;
    // pg_ctl returns when the lock file is gone; the process ends a moment later.
    let deadline = Instant::now() + exit_wait;
    while running() && Instant::now() < deadline {
      tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if running() {
      if !found.own_program {
        return Err(adopted_stop_failed_text(
          found,
          &self.data,
          mode,
          asked.err().as_deref(),
        ));
      }
      self
        .log
        .push("PostgreSQL fast stop did not complete; forcing the identified postmaster");
      process::kill_identified(&found.identity)?;
      let deadline = Instant::now() + Duration::from_secs(10);
      while running() {
        if Instant::now() > deadline {
          return Err("Identified PostgreSQL did not exit; data retained".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
      self
        .log
        .push("Database required a forced stop; crash recovery runs at next launch");
    } else {
      self.log.push(&format!(
        "The earlier database process {pid} stopped after pg_ctl asked it to"
      ));
    }
    Ok(())
  }

  /// Gate fallback uses the same exact-identity shutdown, without starting a server.
  #[cfg(feature = "desktop-gate")]
  pub async fn cleanup_orphan(&self) -> Result<(), String> {
    self.reconcile().await.map(|_| ())
  }

  fn data_arg(&self) -> String {
    cluster_dir(&self.paths.server)
      .join("data")
      .display()
      .to_string()
  }

  pub async fn start(&mut self, masks: &[Arc<LogBuf>]) -> Result<(), String> {
    let note = self.reconcile().await?;
    // PostgreSQL refuses a data directory whose lock file names any live process of this
    // user, whatever that process is (CreateLockFile in its miscinit.c; seen with 18.6:
    // `lock file "postmaster.pid" already exists`). The same function disregards the PID
    // given in PG_GRANDPARENT_PID, which pg_ctl uses to name a process known not to be a
    // competing server. It is given only for a PID just proven not to be a database on
    // this folder (`grandparent_pid`). PostgreSQL still checks the shared memory the lock
    // file names, and it replaces the file itself.
    let grandparent = grandparent_pid(note.as_ref(), cfg!(windows));
    if let Some(pid) = grandparent {
      self.log.push(&format!(
        "PostgreSQL is told that process {pid} is not a database on this folder \
         (PG_GRANDPARENT_PID)"
      ));
    }
    // A supervisor-owned file. No local socket/trust/IPv6/LAN entries.
    fs::write(
      self.data.join("pg_hba.conf"),
      "host all all 127.0.0.1/32 scram-sha-256\n",
    )
    .map_err(io)?;
    for attempt in 0..5 {
      self.port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .and_then(|s| s.local_addr())
        .map_err(io)?
        .port();
      #[cfg(feature = "desktop-gate")]
      if std::env::var("KOMIKA_DESKTOP_GATE_AVOID_PG_PORT")
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
        == Some(self.port)
      {
        continue; // Gate forces a different endpoint without changing production policy.
      }
      for log in masks {
        self.mask_url(log);
      }
      let attempt_log = LogBuf::new();
      self.credentials.mask(std::slice::from_ref(&attempt_log));
      let mut cmd = self.command("postgres");
      // `command` removed any inherited value.
      if let Some(pid) = grandparent {
        cmd.env("PG_GRANDPARENT_PID", pid.to_string());
      }
      cmd.args(["-D", &self.data_arg(), "-p", &self.port.to_string()]);
      for setting in [
        "listen_addresses=127.0.0.1",
        "unix_socket_directories=",
        "shared_buffers=128MB",
        "max_connections=40",
        "wal_compression=on",
        "logging_collector=off",
        "log_destination=stderr",
        "log_min_messages=warning",
        "log_line_prefix=%m ",
        "timezone=UTC",
        "password_encryption=scram-sha-256",
      ] {
        cmd.args(["-c", setting]);
      }
      #[cfg(feature = "desktop-gate")]
      gate_prestart(&self.data, grandparent, self.port);
      let mut child = cmd.spawn().map_err(|e| {
        format!(
          "Could not start {}: {e}",
          self.runtime.bin("postgres").display()
        )
      })?;
      // Not in the Job Object (Windows): it was ended again, and the start stops here.
      crate::child::after_spawn_tied(&mut child, "PostgreSQL", true).await?;
      let mut pumps = Vec::new();
      if let Some(out) = child.stdout.take() {
        pumps.push(pump(out, self.log.clone(), attempt_log.clone()));
      }
      if let Some(err) = child.stderr.take() {
        pumps.push(pump(err, self.log.clone(), attempt_log.clone()));
      }
      self.child = Some(child);
      let ready = self.wait_supervisor().await;
      if ready.is_ok() {
        if let Err(error) = self.provision().await {
          self.stop().await;
          return Err(error);
        }
        self.log.push(&format!(
          "Database ready: authenticated application SELECT 1 on 127.0.0.1:{}",
          self.port
        ));
        return Ok(());
      }
      // Drain exit diagnostics before deciding whether to rebroker the port.
      let dead = self
        .child
        .as_mut()
        .is_some_and(|c| c.try_wait().ok().flatten().is_some());
      if dead {
        for p in pumps {
          let _ = tokio::time::timeout(Duration::from_secs(1), p).await;
        }
      }
      let collision = dead && retry_port(attempt, &attempt_log.text());
      self.stop().await;
      if !collision {
        let said = attempt_log.text();
        // PostgreSQL refused the rights of this app: nothing about the data folder.
        if let Some(refusal) = rights_refusal(&said, &self.data).filter(|_| dead) {
          return Err(refusal);
        }
        // PostgreSQL decided against the lock file: its own words, then what is known.
        let about_lock = dead
          && ["lock file", "postmaster.pid", "shared memory block"]
            .iter()
            .any(|needle| said.contains(needle));
        let lock = self.data.join("postmaster.pid");
        return match &note {
          Some(LockNote::Foreign(foreign)) if about_lock => {
            Err(refusal_text(&said, &lock, foreign))
          }
          Some(LockNote::Unreadable(reason)) if about_lock => {
            Err(unreadable_lock_text(&said, &lock, reason))
          }
          _ => ready,
        };
      }
      self
        .log
        .push("Database port was taken; retrying with a new port");
    }
    Err("Database could not acquire a free loopback port after five attempts".into())
  }

  async fn wait_supervisor(&mut self) -> Result<(), String> {
    let deadline = Instant::now() + READY;
    loop {
      if self.exited()? {
        return Err(format!("PostgreSQL exited while starting {}. The Database log has the cause; correct it, then Retry.", self.data.display()));
      }
      if self.sql("SELECT 1", true, "postgres").await.as_deref() == Ok("1") {
        return Ok(());
      }
      if Instant::now() >= deadline {
        return Err(
          "PostgreSQL did not authenticate within 90 seconds. See the Database log, then Retry."
            .into(),
        );
      }
      tokio::time::sleep(Duration::from_millis(250)).await;
    }
  }

  async fn provision(&self) -> Result<(), String> {
    let role = self
      .sql(
        "SELECT count(*) FROM pg_roles WHERE rolname='komika_app'",
        true,
        "postgres",
      )
      .await?;
    if role == "0" {
      // Password goes only through stdin (never argv). It is constrained to the
      // URL-safe alphabet when read/generated and is registered before any spawn.
      self.sql(&format!("CREATE ROLE komika_app LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD '{}'", self.credentials.application), true, "postgres").await?;
    }
    self
      .sql(
        "ALTER ROLE komika_app NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION",
        true,
        "postgres",
      )
      .await?;
    if self
      .sql(
        "SELECT count(*) FROM pg_database WHERE datname='komika'",
        true,
        "postgres",
      )
      .await?
      == "0"
    {
      self.sql("CREATE DATABASE komika OWNER komika_app TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'", true, "postgres").await?;
    }
    self
      .sql("CREATE EXTENSION IF NOT EXISTS pg_trgm", true, "komika")
      .await?;
    if self.sql("SELECT 1", false, "komika").await? != "1" {
      return Err("Application database authentication did not return SELECT 1".into());
    }
    Ok(())
  }

  pub async fn sql(&self, sql: &str, supervisor: bool, database: &str) -> Result<String, String> {
    self
      .run_tool(
        "psql",
        &vec![
          "-X".into(),
          "-w".into(),
          "-h".into(),
          "127.0.0.1".into(),
          "-p".into(),
          self.port.to_string(),
          "-U".into(),
          if supervisor {
            "komika_supervisor"
          } else {
            "komika_app"
          }
          .into(),
          "-d".into(),
          database.into(),
          "-v".into(),
          "ON_ERROR_STOP=1".into(),
          "-A".into(),
          "-t".into(),
          "-q".into(),
          "-f".into(),
          "-".into(),
        ],
        Some(sql),
        Some(if supervisor {
          &self.credentials.supervisor
        } else {
          &self.credentials.application
        }),
        Duration::from_secs(10),
      )
      .await
  }

  pub fn exited(&mut self) -> Result<bool, String> {
    self
      .child
      .as_mut()
      .map(|child| child.try_wait().map(|s| s.is_some()).map_err(io))
      .unwrap_or(Ok(true))
  }

  async fn stop_identified(&self, identity: &process::Identity) -> Result<(), String> {
    if process::identity(identity.pid)?.as_ref() != Some(identity) {
      return Err(
        "PostgreSQL process identity changed before shutdown; nothing was signaled".into(),
      );
    }
    let pid =
      process::parse_pid(&fs::read_to_string(self.data.join("postmaster.pid")).map_err(io)?)?;
    if !process::owns(identity, &self.runtime.bin("postgres"), &self.data, &pid) {
      return Err("PostgreSQL shutdown ownership could not be proven; nothing was signaled".into());
    }
    let result = self.fast_stop().await;
    if process::identity(identity.pid)?.as_ref() == Some(identity) {
      self
        .log
        .push("PostgreSQL fast stop did not complete; forcing the identified postmaster");
      process::kill_identified(identity)?;
      let deadline = Instant::now() + Duration::from_secs(10);
      while process::identity(identity.pid)?.as_ref() == Some(identity) {
        if Instant::now() > deadline {
          return Err("Identified PostgreSQL did not exit; data retained".into());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
      }
    }
    if result.is_err() {
      self
        .log
        .push("Database required a forced stop; crash recovery runs at next launch");
    }
    Ok(())
  }

  /// `pg_ctl stop -m fast -w`, at most 15 seconds of waiting inside a 20 second bound.
  async fn fast_stop(&self) -> Result<String, String> {
    self.stop_with(StopMode::Fast).await
  }

  async fn stop_with(&self, mode: StopMode) -> Result<String, String> {
    self
      .run_tool(
        "pg_ctl",
        &[
          "-D".into(),
          self.data_arg(),
          "stop".into(),
          "-m".into(),
          mode.as_str().into(),
          "-w".into(),
          "-t".into(),
          "15".into(),
        ],
        None,
        None,
        Duration::from_secs(20),
      )
      .await
  }

  /// Whether `postmaster.pid` names `pid` (the child this supervisor started and holds).
  fn lock_file_names(&self, pid: u32) -> bool {
    fs::read_to_string(self.data.join("postmaster.pid"))
      .ok()
      .and_then(|text| process::parse_pid(&text).ok())
      .is_some_and(|file| file.pid == pid)
  }

  pub async fn stop(&mut self) {
    if let Some(mut child) = self.child.take() {
      if child.try_wait().ok().flatten().is_none() {
        if let Some(pid) = child.id() {
          match inspect_owned(pid) {
            Ok(Some(identity)) => {
              if let Err(error) = self.stop_identified(&identity).await {
                self.log.push(&error);
              }
            }
            other => {
              // The retained handle proves whose child this is even when it cannot be
              // inspected (Windows: the argument query is slow or failed). Ask for an
              // orderly stop first; pg_ctl signals the PID in the lock file, so it is
              // used only when that is this child. The handle is forced only after it.
              let reason = match other {
                Err(error) => error,
                _ => "it reports no identity".into(),
              };
              if self.lock_file_names(pid) {
                self.log.push(&format!(
                  "Could not inspect the database process {pid} ({reason}); asking it to stop with pg_ctl"
                ));
                match self.fast_stop().await {
                  Ok(_) => self.log.push("Database stopped by pg_ctl"),
                  Err(error) => {
                    self.log.push(&error);
                    self.log.push(
                      "pg_ctl could not stop the database; forcing through its retained child handle",
                    );
                    let _ = child.start_kill();
                  }
                }
              } else {
                self.log.push(&format!(
                  "Could not inspect the database process {pid} ({reason}) and the lock file does not name it; forcing through its retained child handle"
                ));
                let _ = child.start_kill();
              }
            }
          }
        }
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
          .await
          .is_err()
        {
          let _ = child.start_kill();
          let _ = child.wait().await;
          self
            .log
            .push("Forced retained PostgreSQL child handle after shutdown timeout");
        }
      }
      self.log.push("Database stopped; data retained");
    }
  }

  pub fn dump_task(&self) -> tokio::task::JoinHandle<()> {
    let worker = Self {
      runtime: self.runtime.clone(),
      root: self.root.clone(),
      data: self.data.clone(),
      paths: self.paths.borrowed(),
      credentials: self.credentials.clone(),
      child: None,
      port: self.port,
      log: self.log.clone(),
    };
    let dir = worker.paths.real_server.join("backups");
    let log = worker.log.clone();
    tokio::spawn(dump_loop(dir, log, RealDumps(worker)))
  }

  /// Serialized by &mut self / the local supervision loop and by `backups/dump.lock`
  /// (which a dump tool left behind by a killed app still holds): never two dumps at once.
  pub async fn dump(&mut self) -> Result<PathBuf, String> {
    self.dump_database("komika").await
  }

  #[cfg(feature = "desktop-gate")]
  pub async fn simulate_dump_failure(&mut self) -> Result<PathBuf, String> {
    self.dump_database("komika_gate_missing_database").await
  }

  async fn dump_database(&mut self, database: &str) -> Result<PathBuf, String> {
    let backups = self.paths.real_server.join("backups");
    fs::create_dir_all(&backups).map_err(io)?;
    // The scheduled task this call replaced may need a moment to let its tool go.
    let waited = Instant::now();
    let lock = loop {
      match DumpLock::acquire(&backups)? {
        Some(lock) => break lock,
        None if waited.elapsed() < Duration::from_secs(5) => {
          tokio::time::sleep(Duration::from_millis(100)).await
        }
        None => {
          return Err(format!(
            "Another backup is still being written ({} is held); this one was skipped",
            backups.join(DUMP_LOCK).display()
          ))
        }
      }
    };
    // Holding the lock proves no dump is being written: these are a killed run's.
    remove_stale_temporaries(&backups);
    let estimate = self
      .sql(
        "SELECT pg_database_size(current_database())",
        true,
        "komika",
      )
      .await?
      .parse::<u64>()
      .map_err(|e| e.to_string())?;
    let required = (estimate.saturating_mul(2)).max(256 * 1024 * 1024);
    if paths::free_bytes(&backups)?.is_some_and(|free| free < required) {
      return Err(format!("Nightly dump skipped: need at least {required} free bytes (twice database size, minimum 256 MiB)"));
    }
    let name = dump_name(Utc::now(), &random_secret()?[..8]);
    let temporary = backups.join(format!("{name}.tmp"));
    let guard = RemoveFile(temporary.clone());
    let tool_path = self
      .paths
      .server
      .join("backups")
      .join(format!("{name}.tmp"));
    self
      .run_tool_holding(
        "pg_dump",
        &[
          "-w".into(),
          "-h".into(),
          "127.0.0.1".into(),
          "-p".into(),
          self.port.to_string(),
          "-U".into(),
          "komika_supervisor".into(),
          "-d".into(),
          database.into(),
          "-Fc".into(),
          "-f".into(),
          tool_path.display().to_string(),
        ],
        None,
        Some(&self.credentials.supervisor),
        Duration::from_secs(3600),
        Some(lock.for_tool()?),
      )
      .await?;
    if fs::metadata(&temporary).map_err(io)?.len() == 0 {
      return Err("Nightly dump was empty; older backups retained".into());
    }
    let final_path = backups.join(name);
    fs::rename(&temporary, &final_path).map_err(io)?;
    drop(guard);
    // Older dumps go only after the new one is in place, complete and not empty.
    if fs::metadata(&final_path).map_err(io)?.len() == 0 {
      return Err("Nightly dump was empty after it was saved; older backups retained".into());
    }
    for removed in retain_dumps(&backups, &final_path, Utc::now())? {
      self
        .log
        .push(&format!("Older dump removed: {}", removed.display()));
    }
    if !final_path.is_file() {
      return Err("Nightly dump is missing after retention".into());
    }
    self
      .log
      .push(&format!("Nightly dump saved: {}", final_path.display()));
    drop(lock);
    Ok(final_path)
  }

  pub async fn inspect_dump(&self, dump: &Path) -> Result<String, String> {
    let relative = dump
      .strip_prefix(&self.paths.real_server)
      .map_err(|e| e.to_string())?;
    self
      .run_tool(
        "pg_restore",
        &[
          "--list".into(),
          self.paths.server.join(relative).display().to_string(),
        ],
        None,
        None,
        Duration::from_secs(30),
      )
      .await
  }
}

/// The identity of the child this supervisor holds. The gate can withhold it to run the
/// stop that a failed inspection takes (the path a slow Windows process query takes).
fn inspect_owned(pid: u32) -> Result<Option<process::Identity>, String> {
  #[cfg(feature = "desktop-gate")]
  if std::env::var_os("KOMIKA_DESKTOP_GATE_BLIND_STOP").is_some_and(|v| v == "1") {
    return Err("inspection withheld by the gate".into());
  }
  process::identity(pid)
}

/// What `postmaster.pid` holds, read without changing it.
#[derive(Debug)]
enum LockRead {
  /// No lock file.
  Absent,
  /// The file exists and names a process.
  File(process::PidFile),
  /// The file exists and is not a usable lock file: empty, cut short, not text, a
  /// process number of 0 or 1, or it could not be read at all. The reason, for the owner.
  Unreadable(String),
}

fn read_lock(path: &Path) -> LockRead {
  let bytes = match fs::read(path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockRead::Absent,
    Err(error) => return LockRead::Unreadable(format!("it could not be read: {error}")),
  };
  if bytes.is_empty() {
    return LockRead::Unreadable("it is empty".into());
  }
  match process::parse_pid(&String::from_utf8_lossy(&bytes)) {
    Ok(pid) => LockRead::File(pid),
    Err(_) => LockRead::Unreadable(
      "its first two lines are not a process number above 1 and a data folder".into(),
    ),
  }
}

/// What the look at the lock file left for the start to know.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LockNote {
  /// The file names a process that is not this folder's database, or not provably.
  Foreign(process::Foreign),
  /// The file is not a usable lock file (the reason).
  Unreadable(String),
}

/// The PID PostgreSQL is told to disregard in the lock file, if any: only one proven
/// not to be a database on this folder (`Foreign::disregard`), and never on Windows.
/// There PostgreSQL's own check asks for the signal pipe that only a PostgreSQL server
/// process has (`pgkill` in its src/port/kill.c), so it already treats the lock file of
/// a process that is not PostgreSQL as left over, and this app could not prove more
/// about a process that is one (the working folder of a process is not read on Windows).
fn grandparent_pid(note: Option<&LockNote>, windows: bool) -> Option<u32> {
  match note {
    Some(LockNote::Foreign(foreign)) if foreign.disregard && !windows => Some(foreign.pid),
    _ => None,
  }
}

/// The Database log line for this folder's database found running (`Claim::Here`).
fn adopted_line(path: &Path, found: &process::Adopted) -> String {
  format!(
    "{} names process {} ({}): this folder's database is still running (its working \
     folder is this data folder{}). An earlier run of the app left it. It is asked to \
     stop with pg_ctl; the lock file is not touched.",
    path.display(),
    found.identity.pid,
    found.identity.image.display(),
    if found.own_program {
      ""
    } else {
      "; its program is not the one this install starts"
    }
  )
}

/// The start page text when this folder's earlier database did not stop for `pg_ctl`
/// and runs another program than this install's, so nothing is forced.
///
/// `pg_ctl stop -m fast` has already sent the request for a fast shutdown (the interrupt
/// signal), so the owner is not told to send that again: first to wait, because a
/// shutdown can take longer than the app waited, and then the command for an immediate
/// end (the quit signal). PostgreSQL ends at once on it, without a last checkpoint, and
/// recovers what was committed from its write-ahead log at the next start.
fn adopted_stop_failed_text(
  found: &process::Adopted,
  data: &Path,
  mode: StopMode,
  pg_ctl_error: Option<&str>,
) -> String {
  let pid = found.identity.pid;
  let (asked, advice) = match mode {
    StopMode::Fast => (
      "asked it to shut down with pg_ctl",
      format!(
        "It may still be shutting down: wait a minute, then Retry. If it is still running \
         then, end it at once with this command in a terminal and Retry; the database \
         recovers what was committed at its next start:\nkill -QUIT {pid}"
      ),
    ),
    // The quit signal was already sent; a process that is deaf to it is ended by force.
    StopMode::Immediate => (
      "told it to quit with pg_ctl",
      format!(
        "End it with this command in a terminal, or restart this computer, then Retry; \
         the database recovers what was committed at its next start:\nkill -KILL {pid}"
      ),
    ),
  };
  format!(
    "Process {pid} ({}) is this app's earlier database process. It is still running on \
     {}. The app {asked} and it did not stop ({}). Nothing was forced and the lock file \
     was not removed. {advice}",
    found.identity.image.display(),
    data.display(),
    pg_ctl_error.unwrap_or("pg_ctl reported success, but the process is still there")
  )
}

/// A `pg_ctl stop` mode. `Fast`: the interrupt signal; PostgreSQL ends its sessions and
/// shuts down with a checkpoint. `Immediate`: the quit signal; PostgreSQL ends at once,
/// without a checkpoint, and recovers from its write-ahead log at the next start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopMode {
  Fast,
  Immediate,
}
impl StopMode {
  fn as_str(self) -> &'static str {
    match self {
      StopMode::Fast => "fast",
      StopMode::Immediate => "immediate",
    }
  }
}

/// How this folder's earlier database, started with `args`, is stopped. PostgreSQL
/// writes its control file by the path it was started with (`-D`). Ordinarily that is
/// this data folder, `data`, and a shutdown is asked for. When the folder was renamed
/// since, the path names another place; when a control file lies there and is not this
/// folder's own, a shutdown checkpoint would overwrite it, so the mode is `Immediate`.
fn adopted_stop_mode(args: &[String], data: &Path) -> StopMode {
  let control = |folder: &Path| folder.join("global").join("pg_control").canonicalize();
  let started_on = args
    .windows(2)
    .filter(|pair| pair[0] == "-D")
    .map(|pair| PathBuf::from(&pair[1]))
    .next_back();
  match started_on {
    Some(started_on) if started_on.is_absolute() => match (control(&started_on), control(data)) {
      (Ok(theirs), Ok(ours)) if theirs == ours => StopMode::Fast,
      (Ok(_), _) => StopMode::Immediate,
      (Err(_), _) => StopMode::Fast,
    },
    _ => StopMode::Fast,
  }
}

/// How long this folder's earlier database may take to end after `pg_ctl stop` has
/// returned (see [`Database::stop_adopted`]).
const ADOPTED_EXIT_WAIT: Duration = Duration::from_secs(5);

/// How long a PostgreSQL that runs in another folder gets to end: after the request to
/// shut down, and after the order to quit ([`end_strays`]).
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct StrayWaits {
  interrupt: Duration,
  quit: Duration,
}
#[cfg(unix)]
const STRAY_WAITS: StrayWaits = StrayWaits {
  interrupt: Duration::from_secs(15),
  quit: Duration::from_secs(10),
};

#[cfg(unix)]
fn stray_named(process: &process::Seen) -> String {
  format!("Process {} ({})", process.pid, process.program.display())
}

/// The Database log line for a PostgreSQL of this data path that runs in another folder.
#[cfg(unix)]
fn stray_line(process: &process::Seen, data: &Path, folder: &Path, quit_at_once: bool) -> String {
  format!(
    "{} is a PostgreSQL that was started on this data path ({}) and now runs in another \
     folder ({}): the folder it was started in was renamed or replaced while it ran. The \
     lock file in the folder it runs in names this process, this data path and the \
     time this process was started. \
     PostgreSQL writes its control file by the path it was started with, so it would \
     write into the database at this path. It is ended before anything is created or \
     started here: {}.",
    stray_named(process),
    data.display(),
    folder.display(),
    if quit_at_once {
      "it is told to quit at once (the quit signal) and not asked to shut down, because a \
       database lies at this path and a shutdown would write that control file"
    } else {
      "it is asked to shut down (the interrupt signal) and told to quit if it does not"
    }
  )
}

/// The start page text for a process with a PostgreSQL name that has to do with this data
/// path and is not proven to be a database that endangers it (`reason`): it is not
/// signaled and nothing is started.
#[cfg(unix)]
fn stray_unproven_text(process: &process::Seen, data: &Path, reason: &str) -> String {
  let pid = process.pid;
  format!(
    "{} has the name of a PostgreSQL program and has to do with this data path ({}), \
     and the app could not prove what it is: {reason}. A PostgreSQL that was started on \
     this path and runs in another folder writes its control file into the database at \
     this path, so the app created and started nothing here. It did not signal process \
     {pid}: only a process that the lock file in its own folder proves to be such a \
     database is ever ended by the app. End process {pid} yourself, or restart this \
     computer, then Retry.",
    stray_named(process),
    data.display()
  )
}

/// How long a process whose proof no longer holds gets to be gone by itself before the
/// start is refused (a server that shuts down removes its lock file just before it ends).
#[cfg(unix)]
const STRAY_PROOF_GRACE: Duration = Duration::from_secs(2);

/// The start page text for a process that the look took for a PostgreSQL of this data
/// path, and for which the proof did not hold any more when a signal was due (`why`).
#[cfg(unix)]
fn stray_changed_text(process: &process::Seen, data: &Path, why: &str) -> String {
  let pid = process.pid;
  format!(
    "{} was found as a PostgreSQL that was started on this data path ({}) and runs in \
     another folder, but when the app was about to signal it, that was no longer proven: \
     {why}. The app sent it nothing at that moment, and created and started nothing \
     here. If it is such a database it can write its control file into the database at \
     this path. End process {pid} yourself, or restart this computer, then Retry.",
    stray_named(process),
    data.display()
  )
}

/// The start page text for a PostgreSQL of this data path that runs in another folder
/// and could not be signaled.
#[cfg(unix)]
fn stray_unsignaled_text(process: &process::Seen, data: &Path, error: &str) -> String {
  let pid = process.pid;
  format!(
    "{} is a PostgreSQL that was started on this data path ({}) and runs in another \
     folder. The app could not end it ({error}). It can write its control file into the \
     database at this path, so the app created and started nothing here. End process \
     {pid} or restart this computer, then Retry.",
    stray_named(process),
    data.display()
  )
}

/// The start page text for a PostgreSQL of this data path, or one of its helper
/// processes, that is still running after both signals.
#[cfg(unix)]
fn stray_alive_text(process: &process::Seen, data: &Path, still: &[u32]) -> String {
  let pid = process.pid;
  format!(
    "{} is a PostgreSQL that was started on this data path ({}) and runs in another \
     folder. It was told to quit and has not ended (still running: process {}). It can \
     write its control file into the database at this path, so the app created and \
     started nothing here. End process {pid} (in a terminal: kill -KILL {pid}) or \
     restart this computer, then Retry.",
    stray_named(process),
    data.display(),
    still
      .iter()
      .map(u32::to_string)
      .collect::<Vec<_>>()
      .join(", ")
  )
}

/// The look before a start. Find every live PostgreSQL of this user that was started on
/// the data path `data` and runs in another folder, and end it, before this app creates
/// or starts a cluster at that path.
///
/// How this comes about on macOS, where nothing ends the database with the app: the app
/// is force-quit, the owner renames the app data folder aside, and the app is started
/// again at the old name. The database of the renamed folder is still alive. It works in
/// its renamed folder, and writes its control file by the old path: into the new cluster.
/// The lock file cannot show it, because the lock file moved away with the folder. So
/// the processes themselves are looked at ([`process::strays`]), not a file.
///
/// A process scan, and no record of the child kept outside the app data folder, because
/// the scan reads what is true now and needs nothing that can be lost or go stale: it
/// also finds a database left by an app version from before this check, and one started
/// by hand on this path. A record would know only the databases this version started.
///
/// The order of ending: a request to shut down (interrupt), a bounded wait, then the
/// order to quit, a bounded wait. With one exception: when a control file lies at the
/// data path, the request to shut down is left out. A shutdown ends with a checkpoint
/// that writes the control file by the old path, which is the very damage this function
/// is there to prevent; the order to quit ends PostgreSQL without a checkpoint. Its own
/// folder loses nothing either way: what was committed is in its write-ahead log and is
/// recovered when that folder is next opened.
///
/// A signal is sent only to a process that the lock file in its own working folder
/// proves to be such a database ([`process::strays`]). Its name, its `-D` argument and
/// its `PGDATA` prove nothing: a process that only those tie to this data path, and one
/// whose working folder cannot be read, is never signaled, and the start is refused with
/// its process number. Nothing is ever sent to a process that runs in this folder (that
/// is this folder's database, handled by [`Database::reconcile`]) or to one that is not
/// proven again, by its creation time, right before each signal.
///
/// What the look cannot see: a process of another user, a process whose entry in the
/// process list could not be read at all, and a database that was started with a
/// relative data path and whose lock file is gone. An empty look is not a proof that no
/// such process exists.
#[cfg(unix)]
async fn end_strays(data: &Path, log: &LogBuf, waits: StrayWaits) -> Result<(), String> {
  end_found(data, log, waits, look_for_strays(data)).await
}

/// The look itself, on the live processes of this user.
#[cfg(unix)]
fn look_for_strays(data: &Path) -> Vec<process::Stray> {
  process::strays(
    data,
    &process::postgres_processes(),
    |pid| process::place_before_start(pid, data),
    process::lock_where_it_runs,
  )
}

/// [`end_strays`] for the processes that the look found.
#[cfg(unix)]
async fn end_found(
  data: &Path,
  log: &LogBuf,
  waits: StrayWaits,
  strays: Vec<process::Stray>,
) -> Result<(), String> {
  let mut unread = Vec::new();
  for stray in strays {
    let (process, folder, evidence, helpers) = match stray {
      process::Stray::Unproven { process, reason } => {
        unread.push(stray_unproven_text(&process, data, &reason));
        continue;
      }
      process::Stray::Elsewhere {
        process,
        folder,
        evidence,
        helpers,
      } => (process, folder, evidence, helpers),
    };
    // A cluster at this path: a shutdown checkpoint of that process would land in it.
    let quit_at_once = data.join("global").join("pg_control").exists();
    log.push(&stray_line(&process, data, &folder, quit_at_once));
    let gone = |bound: Duration| {
      let (pid, created) = (process.pid, process.created.clone());
      async move {
        let deadline = Instant::now() + bound;
        while process::still(pid, &created) {
          if Instant::now() >= deadline {
            return false;
          }
          tokio::time::sleep(Duration::from_millis(50)).await;
        }
        true
      }
    };
    // Before EVERY signal the whole proof is made again, not only the creation time:
    // the look can be seconds old by now (the waits for an earlier process, the wait
    // between the interrupt and the quit signal), and in that time the lock file can
    // have gone, the folder can have been replaced, or the process can run something
    // else. Proof that no longer holds refuses the start with the process and sends
    // nothing. One case is no refusal: a server that is shutting down removes its lock
    // file a moment before it ends, so a process whose proof is gone gets a short time
    // to be gone itself. A signal that could not be sent refuses the start as well.
    let signal = |signal: libc::c_int| {
      let (process, folder, evidence, gone) = (&process, &folder, &evidence, &gone);
      async move {
        if !process::still(process.pid, &process.created) {
          return Ok(false);
        }
        if let Err(why) = process::prove_again(data, process, folder, evidence) {
          if gone(STRAY_PROOF_GRACE).await {
            return Ok(false);
          }
          return Err(stray_changed_text(process, data, &why));
        }
        process::signal_created(process.pid, &process.created, signal)
          .map_err(|error| stray_unsignaled_text(process, data, &error))
      }
    };
    let mut sent = Vec::new();
    let mut ended = false;
    if !quit_at_once {
      if signal(libc::SIGINT).await? {
        sent.push("interrupt");
      }
      ended = gone(waits.interrupt).await;
    }
    if !ended {
      if signal(libc::SIGQUIT).await? {
        sent.push("quit");
      }
      ended = gone(waits.quit).await;
    }
    // Its helper processes end by themselves once the main process is gone. The
    // checkpointer is one of them, so the start waits for them too.
    let deadline = Instant::now() + waits.quit;
    let left = loop {
      let left: Vec<u32> = helpers
        .iter()
        .filter(|helper| process::still(helper.pid, &helper.created))
        .map(|helper| helper.pid)
        .chain((!ended).then_some(process.pid))
        .collect();
      if left.is_empty() || Instant::now() >= deadline {
        break left;
      }
      tokio::time::sleep(Duration::from_millis(50)).await;
    };
    #[cfg(feature = "desktop-gate")]
    gate_event(
      "stray",
      data,
      serde_json::json!({"pid": process.pid, "program": process.program, "signals": sent,
        "runs_in": folder, "helpers": helpers.iter().map(|h| h.pid).collect::<Vec<_>>(),
        "control_file_at_the_data_path_before": quit_at_once, "still_running": left}),
    );
    if !left.is_empty() {
      return Err(stray_alive_text(&process, data, &left));
    }
    log.push(&format!(
      "{} has ended ({}). Its own folder keeps its data; what was committed there is \
       recovered from the write-ahead log when that folder is next opened.",
      stray_named(&process),
      if sent.is_empty() {
        "it ended by itself".to_string()
      } else {
        format!("signals sent: {}", sent.join(", then "))
      }
    ));
  }
  match unread.into_iter().next() {
    Some(refusal) => Err(refusal),
    None => Ok(()),
  }
}

/// PostgreSQL's own FATAL, PANIC, DETAIL and HINT lines of a failed start, in one line.
fn postgres_said(postgres_output: &str) -> String {
  let mut said = Vec::new();
  for line in postgres_output.lines() {
    for level in ["FATAL:", "PANIC:", "DETAIL:", "HINT:"] {
      if let Some(at) = line.find(level) {
        said.push(line[at..].split_whitespace().collect::<Vec<_>>().join(" "));
        break;
      }
    }
  }
  if said.is_empty() {
    "it gave no reason; see the Database log".to_string()
  } else {
    said.join(" ")
  }
}

/// A start that PostgreSQL refused over a lock file that is not a usable lock file:
/// PostgreSQL's own message, the file, and what the owner can do. The app never moves or
/// removes the file itself.
fn unreadable_lock_text(postgres_output: &str, path: &Path, reason: &str) -> String {
  format!(
    "PostgreSQL did not start. It said: {}\nThe lock file {} is not a usable lock file \
     ({reason}). The app did not change or remove it and stopped nothing. It can be what \
     is left of a start that was cut off. First make sure that no `postgres` process is \
     running on this data folder (Activity Monitor or Task Manager; in a terminal: ps ax \
     | grep postgres). Then move this one file aside, for example to {}.aside, and Retry.",
    postgres_said(postgres_output),
    path.display(),
    path.display()
  )
}

/// The one line the Database log gets for a lock file whose PID is not ours.
fn stale_pid_line(path: &Path, foreign: &process::Foreign) -> String {
  format!(
    "{} names {}: {}. The file is treated as left over from an earlier run. That process \
     was not stopped and the file was not removed; PostgreSQL decides at start.",
    path.display(),
    foreign.named(),
    foreign.what
  )
}

/// A start that PostgreSQL refused while the lock file named a foreign process:
/// PostgreSQL's own message, then the process.
fn refusal_text(postgres_output: &str, path: &Path, foreign: &process::Foreign) -> String {
  format!(
    "PostgreSQL did not start. It said: {}\nThe lock file {} names {}: {}. That \
     process was not stopped and the lock file was not removed. Quit that program, or \
     restart this computer, then Retry.",
    postgres_said(postgres_output),
    path.display(),
    foreign.named(),
    foreign.what
  )
}

/// Gate only: an existing folder in the form the app itself works with and prints
/// (`prepare` does the same with the data folder). On Windows that is the plain `C:\…`
/// form with `\` alone; the gate is given its folder with both separators there.
#[cfg(feature = "desktop-gate")]
pub fn gate_work_path(path: &Path) -> Result<PathBuf, String> {
  path.canonicalize().map(paths::tool_path).map_err(io)
}

/// What the lock file is at this moment: whether it exists, which file it is (inode,
/// Unix) and the SHA-256 of its content. The gate compares it before a start with the
/// moment PostgreSQL is spawned.
#[cfg(feature = "desktop-gate")]
pub fn gate_lock_state(data: &Path) -> serde_json::Value {
  let path = data.join("postmaster.pid");
  let bytes = match fs::read(&path) {
    Ok(bytes) => bytes,
    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
      return serde_json::json!({"exists": false})
    }
    Err(error) => return serde_json::json!({"exists": true, "error": error.to_string()}),
  };
  #[cfg(unix)]
  let inode = {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(&path).map(|meta| meta.ino()).ok()
  };
  #[cfg(not(unix))]
  let inode: Option<u64> = None;
  let text = String::from_utf8_lossy(&bytes);
  serde_json::json!({
    "exists": true,
    "inode": inode,
    "sha256": format!("{:x}", Sha256::digest(&bytes)),
    "lines": text.lines().count(),
    "first_line": text.lines().next(),
  })
}

/// Gate only: one JSON line per PostgreSQL spawn (`"event":"spawn"`), written right
/// before it, into the file named by `KOMIKA_DESKTOP_GATE_PRESTART`: the lock file as it
/// is at that moment and the PID given in PG_GRANDPARENT_PID (`null`: the variable is not
/// passed).
#[cfg(feature = "desktop-gate")]
fn gate_prestart(data: &Path, grandparent: Option<u32>, port: u16) {
  gate_event(
    "spawn",
    data,
    serde_json::json!({
      "lock_file": gate_lock_state(data),
      "grandparent_pid": grandparent,
      "port": port,
    }),
  );
}

/// The SHA-256 of the control file of the cluster at `data`, as it is now on disk.
/// `None`: there is no such file. The gate compares it over a start.
#[cfg(feature = "desktop-gate")]
pub fn gate_control_file(data: &Path) -> Option<String> {
  fs::read(data.join("global").join("pg_control"))
    .ok()
    .map(|bytes| format!("{:x}", Sha256::digest(&bytes)))
}

/// Gate only: one JSON line for the moment `event` into the file named by
/// `KOMIKA_DESKTOP_GATE_PRESTART`. Every line also says what is true at that moment for
/// the data path: the control file of the cluster that lies there (its hash), and the
/// process numbers of every live PostgreSQL that was started on this path and runs in
/// another folder (`started_here_running_elsewhere`; the look of `end_strays`, made
/// again).
#[cfg(feature = "desktop-gate")]
fn gate_event(event: &str, data: &Path, mut record: serde_json::Value) {
  let Some(path) = std::env::var_os("KOMIKA_DESKTOP_GATE_PRESTART") else {
    return;
  };
  #[cfg(unix)]
  let elsewhere: Vec<u32> = look_for_strays(data)
    .into_iter()
    .map(|stray| match stray {
      process::Stray::Elsewhere { process, .. } | process::Stray::Unproven { process, .. } => {
        process.pid
      }
    })
    .collect();
  #[cfg(not(unix))]
  let elsewhere: Vec<u32> = Vec::new();
  record["event"] = event.into();
  record["control_file_sha256"] = gate_control_file(data).into();
  record["started_here_running_elsewhere"] = elsewhere.into();
  if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
    let _ = writeln!(file, "{record}");
  }
}

#[cfg(feature = "desktop-gate")]
impl Database {
  /// The gate's restore check: a new empty database in this cluster, made like the
  /// application database, and `pg_restore` of `dump` into it. Returns its name.
  pub async fn restore_dump(&self, dump: &Path) -> Result<String, String> {
    let relative = dump
      .strip_prefix(&self.paths.real_server)
      .map_err(|e| e.to_string())?;
    let name = format!("komika_restore_{}", &random_secret()?[..12]);
    self
      .sql(
        &format!(
          "CREATE DATABASE {name} OWNER komika_app TEMPLATE template0 ENCODING 'UTF8' \
           LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'"
        ),
        true,
        "postgres",
      )
      .await?;
    self
      .run_tool(
        "pg_restore",
        &[
          "-w".into(),
          "--exit-on-error".into(),
          "-h".into(),
          "127.0.0.1".into(),
          "-p".into(),
          self.port.to_string(),
          "-U".into(),
          "komika_supervisor".into(),
          "-d".into(),
          name.clone(),
          self.paths.server.join(relative).display().to_string(),
        ],
        None,
        Some(&self.credentials.supervisor),
        Duration::from_secs(600),
      )
      .await?;
    Ok(name)
  }

  /// The program file this supervisor starts as the database: the gate tells this
  /// install's database processes from others by it.
  pub fn postgres_program(&self) -> PathBuf {
    self.runtime.bin("postgres")
  }
}

/// The program file of a live process, for the gate's orphan check.
#[cfg(feature = "desktop-gate")]
pub fn gate_process_image(pid: u32) -> Result<Option<PathBuf>, String> {
  process::image(pid)
}

/// When the live process `pid` was created, as an opaque text: the gate tells a process
/// it left running from a later process with the same number. `None`: no such process.
#[cfg(feature = "desktop-gate")]
pub fn gate_process_created(pid: u32) -> Option<String> {
  process::identity(pid).ok().flatten().map(|i| i.created)
}

struct RemoveFile(PathBuf);
impl Drop for RemoveFile {
  fn drop(&mut self) {
    let _ = fs::remove_file(&self.0);
  }
}

pub fn check_cluster(data: &Path) -> Result<(), String> {
  let version = fs::read_to_string(data.join("PG_VERSION")).map_err(|_| format!("Partly initialized database at {} (PG_VERSION missing). Preserve this directory; recover or move it aside yourself, then Retry.", data.display()))?;
  if version.trim() != MAJOR {
    return Err(wrong_major(data, version.trim(), MAJOR));
  }
  for required in ["base", "global", "pg_wal", "postgresql.conf", "pg_hba.conf"] {
    if !data.join(required).exists() {
      return Err(format!(
        "Partly initialized database at {}: {required} missing. Recover the directory, then Retry.",
        data.display()
      ));
    }
  }
  Ok(())
}

fn pump<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
  reader: R,
  log: Arc<LogBuf>,
  attempt: Arc<LogBuf>,
) -> tokio::task::JoinHandle<()> {
  tokio::spawn(async move {
    let mut reader = tokio::io::BufReader::new(reader);
    let mut bytes = Vec::new();
    while reader
      .read_until(b'\n', &mut bytes)
      .await
      .is_ok_and(|n| n > 0)
    {
      let line = String::from_utf8_lossy(&bytes);
      log.push(line.trim_end());
      attempt.push(line.trim_end());
      bytes.clear();
    }
  })
}

/// A good dump is at least this old before the next one is made.
pub const DUMP_INTERVAL: Duration = Duration::from_secs(20 * 3600);
/// A dump dated at most this far ahead of the clock is an ordinary recent dump.
pub const DUMP_FUTURE_TOLERANCE: Duration = Duration::from_secs(300);
/// Completed dumps kept.
pub const DUMP_KEEP: usize = 7;
const DUMP_LOCK: &str = "dump.lock";
const DUMP_SCHEDULE: &str = "dump-schedule.json";

/// One completed dump and the UTC time recorded in its own name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DumpFile {
  pub path: PathBuf,
  pub at: DateTime<Utc>,
}

pub fn dump_name(now: DateTime<Utc>, suffix: &str) -> String {
  format!("komika-{}-{suffix}.dump", now.format("%Y%m%dT%H%M%SZ"))
}

/// `komika-20261011T020000Z-1a2b3c4d.dump` → that UTC time.
fn recorded_time(name: &str) -> Option<DateTime<Utc>> {
  let rest = name.strip_prefix("komika-")?.strip_suffix(".dump")?;
  let stamp = rest.get(..16)?;
  if !(rest.len() == 16 || rest[16..].starts_with('-')) {
    return None;
  }
  NaiveDateTime::parse_from_str(stamp, "%Y%m%dT%H%M%SZ")
    .ok()
    .map(|t| t.and_utc())
}

/// Completed dumps, newest first by the time in their own name. A dump whose name holds
/// no time (not written by this app) is ordered by its modification time.
pub fn dump_files(dir: &Path) -> Result<Vec<DumpFile>, String> {
  if !dir.exists() {
    return Ok(Vec::new());
  }
  let mut files = Vec::new();
  for entry in fs::read_dir(dir).map_err(io)? {
    let entry = entry.map_err(io)?;
    let name = entry.file_name().to_string_lossy().to_string();
    if name.starts_with("komika-")
      && name.ends_with(".dump")
      && entry.file_type().map_err(io)?.is_file()
    {
      let at = recorded_time(&name).unwrap_or_else(|| {
        entry
          .metadata()
          .and_then(|m| m.modified())
          .map(DateTime::<Utc>::from)
          .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
      });
      files.push(DumpFile {
        path: entry.path(),
        at,
      });
    }
  }
  files.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.path.cmp(&a.path)));
  Ok(files)
}

pub fn dumps(dir: &Path) -> Result<Vec<PathBuf>, String> {
  Ok(dump_files(dir)?.into_iter().map(|f| f.path).collect())
}

/// Keep [`DUMP_KEEP`] completed dumps in all and remove the rest. The order of keeping:
/// `just_written` (never removed, wherever its time sorts), then the dumps that are not
/// dated in the future, newest first, and only then, for places still free, the dumps
/// dated in the future (beyond [`DUMP_FUTURE_TOLERANCE`] after `now`), newest first. A
/// dump with a wrong date from a clock that ran ahead never pushes out a correctly dated
/// one.
pub fn retain_dumps(
  dir: &Path,
  just_written: &Path,
  now: DateTime<Utc>,
) -> Result<Vec<PathBuf>, String> {
  let written = fs::metadata(just_written).map_err(|e| {
    format!(
      "The new dump {} is not in place ({e}); no older dump was removed",
      just_written.display()
    )
  })?;
  if !written.is_file() || written.len() == 0 {
    return Err("The new dump is empty; no older dump was removed".into());
  }
  let horizon = now + chrono::Duration::from_std(DUMP_FUTURE_TOLERANCE).expect("small duration");
  // `dump_files` is newest first; a stable partition keeps that order in both halves.
  let (dated, ahead): (Vec<DumpFile>, Vec<DumpFile>) = dump_files(dir)?
    .into_iter()
    .filter(|file| file.path.file_name() != just_written.file_name())
    .partition(|file| file.at <= horizon);
  let mut kept = 1;
  let mut removed = Vec::new();
  for file in dated.into_iter().chain(ahead) {
    if kept < DUMP_KEEP {
      kept += 1;
      continue;
    }
    fs::remove_file(&file.path).map_err(io)?;
    removed.push(file.path);
  }
  Ok(removed)
}

/// Unfinished dumps of a run that was killed. Call only while holding the dump lock.
fn remove_stale_temporaries(dir: &Path) {
  for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
    let name = entry.file_name().to_string_lossy().to_string();
    if name.starts_with("komika-") && name.ends_with(".dump.tmp") {
      let _ = fs::remove_file(entry.path());
    }
  }
}

/// Held while a dump is written. The dump tool inherits it, so it stays held for as long
/// as either the app's dump or a tool left by a killed app is alive.
pub struct DumpLock {
  file: File,
}
impl DumpLock {
  /// `None`: another dump holds it.
  pub fn acquire(dir: &Path) -> Result<Option<Self>, String> {
    let path = dir.join(DUMP_LOCK);
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
      use std::os::windows::fs::OpenOptionsExt;
      options.share_mode(0);
    }
    let mut file = match options.open(&path) {
      Ok(file) => file,
      // ERROR_SHARING_VIOLATION: the exclusive open of the holder.
      #[cfg(windows)]
      Err(error) if error.raw_os_error() == Some(32) => return Ok(None),
      Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    #[cfg(unix)]
    {
      use std::os::fd::AsRawFd;
      // SAFETY: flock borrows a live descriptor. The lock belongs to the open file, so a
      // duplicate handed to the dump tool keeps it until the tool ends too.
      if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(None);
      }
    }
    use std::io::{Seek, SeekFrom};
    let _ = file.set_len(0);
    let _ = file.seek(SeekFrom::Start(0));
    let _ = writeln!(
      file,
      "pid {} started {}",
      std::process::id(),
      Utc::now().format("%Y%m%dT%H%M%SZ")
    );
    Ok(Some(Self { file }))
  }

  fn for_tool(&self) -> Result<File, String> {
    self.file.try_clone().map_err(io)
  }
}

/// What is kept between launches about a newest dump dated in the future.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct FutureDump {
  /// File name of the newest future-dated dump.
  name: String,
  /// Seconds the app has run since that dump was first seen to be in the future.
  running_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DumpDecision {
  /// No dump, or the newest usable one is at least [`DUMP_INTERVAL`] old.
  Due,
  /// A dump younger than [`DUMP_INTERVAL`] exists.
  Recent,
  /// The newest dump is dated in the future. It counts as a good recent dump until the
  /// app has run for one [`DUMP_INTERVAL`]; `remaining` of that is left.
  FutureDated { name: String, remaining: Duration },
}

/// The schedule rule, on plain values. `running` is the time the app has run since the
/// previous call (monotonic, so a clock change cannot move it).
///
/// Newest dump not in the future → due when it is at least 20 hours old. No dump → due.
/// Newest dump in the future beyond the tolerance → the clock was changed: wait until
/// the app has run 20 hours since that was first seen, then stop counting that dump as
/// recent (it stays on disk) and apply the rule to the dumps that are not in the future.
fn dump_decision(
  files: &[DumpFile],
  now: DateTime<Utc>,
  future: &mut Option<FutureDump>,
  running: Duration,
) -> DumpDecision {
  let horizon = now + chrono::Duration::from_std(DUMP_FUTURE_TOLERANCE).expect("small duration");
  // `files` is newest first.
  let ahead = files.iter().find(|f| f.at > horizon);
  match ahead {
    Some(ahead) => {
      let name = ahead
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
      let mut state = future
        .take()
        .filter(|state| state.name == name)
        .unwrap_or(FutureDump {
          name: name.clone(),
          running_seconds: 0,
        });
      state.running_seconds = state.running_seconds.saturating_add(running.as_secs());
      let ran = Duration::from_secs(state.running_seconds);
      *future = Some(state);
      if ran < DUMP_INTERVAL {
        return DumpDecision::FutureDated {
          name,
          remaining: DUMP_INTERVAL - ran,
        };
      }
    }
    None => *future = None,
  }
  match files.iter().find(|f| f.at <= horizon) {
    None => DumpDecision::Due,
    Some(newest) => {
      let age = now.signed_duration_since(newest.at);
      if age >= chrono::Duration::from_std(DUMP_INTERVAL).expect("small duration") {
        DumpDecision::Due
      } else {
        DumpDecision::Recent
      }
    }
  }
}

/// How often the dump task looks at the backups folder.
pub const DUMP_POLL: Duration = Duration::from_secs(30);
/// A dump that failed is tried again after this much running time, not sooner.
pub const DUMP_RETRY: Duration = Duration::from_secs(3600);

/// The decision path of the dump task (`Database::dump_task`), apart from the timer and
/// the dump itself so that both can be given to it by a test.
pub struct DumpTicker {
  schedule: DumpSchedule,
  /// When a dump was last tried, on the monotonic clock.
  attempted: Option<Instant>,
}
impl DumpTicker {
  pub fn new(dir: PathBuf) -> Self {
    Self {
      schedule: DumpSchedule::new(dir),
      attempted: None,
    }
  }

  /// One look at the folder: wall-clock time `now`, `running` after the previous look,
  /// monotonic time `at`. Calls `dump` when a dump is due and none was tried within
  /// [`DUMP_RETRY`]. Returns whether it was called.
  pub async fn tick<F, Fut>(
    &mut self,
    now: DateTime<Utc>,
    running: Duration,
    at: Instant,
    log: &LogBuf,
    dump: F,
  ) -> bool
  where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<PathBuf, String>>,
  {
    let due = self.schedule.poll(now, running, log) == DumpDecision::Due;
    if !due
      || self
        .attempted
        .is_some_and(|tried| at.saturating_duration_since(tried) < DUMP_RETRY)
    {
      return false;
    }
    self.attempted = Some(at);
    if let Err(error) = dump().await {
      log.push(&format!("Nightly dump failed: {error}"));
    }
    true
  }
}

/// What the dump task takes from the outside: the two clocks, its pause between looks,
/// and the dump itself. The app gives it the real ones ([`RealDumps`]); a test gives it
/// a clock it moves and a dump it counts.
trait DumpRun {
  /// The wall clock and the monotonic clock, read together.
  fn now(&mut self) -> (DateTime<Utc>, Instant);
  async fn pause(&mut self, time: Duration);
  async fn dump(&mut self) -> Result<PathBuf, String>;
}

struct RealDumps(Database);
impl DumpRun for RealDumps {
  fn now(&mut self) -> (DateTime<Utc>, Instant) {
    (Utc::now(), Instant::now())
  }
  async fn pause(&mut self, time: Duration) {
    tokio::time::sleep(time).await
  }
  async fn dump(&mut self) -> Result<PathBuf, String> {
    self.0.dump().await
  }
}

/// The dump task: for ever, one look at the backups folder `dir` ([`DumpTicker::tick`]),
/// then a pause of [`DUMP_POLL`]. It is looked at again after sleep/resume or a clock
/// change. A failed dump is tried again at most hourly; a good one is at least
/// [`DUMP_INTERVAL`] old before the next.
async fn dump_loop<R: DumpRun>(dir: PathBuf, log: Arc<LogBuf>, mut run: R) {
  let mut ticker = DumpTicker::new(dir);
  let mut polled = run.now().1;
  loop {
    // Running time since the last look, never the wall clock: it cannot jump.
    let (wall, now) = run.now();
    let running = now.saturating_duration_since(polled);
    polled = now;
    ticker.tick(wall, running, now, &log, || run.dump()).await;
    run.pause(DUMP_POLL).await;
  }
}

/// The dump schedule of one backups folder. Its only memory between launches is the
/// folder: the dumps' own names and `dump-schedule.json`.
pub struct DumpSchedule {
  dir: PathBuf,
  warned: Option<String>,
}
impl DumpSchedule {
  pub fn new(dir: PathBuf) -> Self {
    Self { dir, warned: None }
  }

  /// One look at the folder at wall-clock time `now`, `running` after the previous look.
  pub fn poll(&mut self, now: DateTime<Utc>, running: Duration, log: &LogBuf) -> DumpDecision {
    let files = match dump_files(&self.dir) {
      Ok(files) => files,
      // Unreadable folder: let the dump try, so its failure reaches the Database log.
      Err(_) => return DumpDecision::Due,
    };
    let path = self.dir.join(DUMP_SCHEDULE);
    let before: Option<FutureDump> = fs::read(&path)
      .ok()
      .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let mut state = before.clone();
    let decision = dump_decision(&files, now, &mut state, running);
    if state != before {
      match &state {
        Some(state) => {
          let temporary = self.dir.join(format!("{DUMP_SCHEDULE}.tmp"));
          let saved = serde_json::to_vec(state)
            .map_err(|e| e.to_string())
            .and_then(|bytes| fs::write(&temporary, bytes).map_err(io))
            .and_then(|()| fs::rename(&temporary, &path).map_err(io));
          if saved.is_err() {
            let _ = fs::remove_file(temporary);
          }
        }
        None => {
          let _ = fs::remove_file(&path);
        }
      }
    }
    if let Some(state) = &state {
      if self.warned.as_deref() != Some(state.name.as_str()) {
        self.warned = Some(state.name.clone());
        log.push(&format!(
          "Warning: the newest dump {} is dated in the future, so this computer's clock was \
           changed. The dump is kept. The next dump is made once the app has run for {} hours \
           since this was first noticed.",
          state.name,
          DUMP_INTERVAL.as_secs() / 3600
        ));
      }
    } else {
      self.warned = None;
    }
    decision
  }
}

#[cfg(test)]
mod tests;
