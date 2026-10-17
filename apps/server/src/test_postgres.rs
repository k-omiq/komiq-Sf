//! Every app test gets a native PostgreSQL 18 database cloned from a migrated template.
//! This cluster has a private socket directory, no TCP listener, and no connection
//! to any pre-existing database. `empty_database` clones an empty template0
//! database instead. A pipe-watching child cleans up on process exit.
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::str::FromStr;
use std::sync::{Mutex, OnceLock};

struct Cluster {
    root: PathBuf,
    bin: PathBuf,
    port: u16,
    template: String,
    url: String,
    lifecycle: Mutex<()>,
    // EOF when the test executable exits makes the watchdog stop OUR cluster.
    _watchdog: Mutex<ChildStdin>,
}
static CLUSTER: OnceLock<Result<Cluster, String>> = OnceLock::new();

#[derive(Debug)]
pub struct DisposableDatabase {
    pub url: String,
    name: String,
}
impl Drop for DisposableDatabase {
    fn drop(&mut self) {
        if let Some(Ok(cluster)) = CLUSTER.get() {
            let _guard = cluster.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
            let _ = cluster.psql(
                "postgres",
                &format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name),
            );
        }
    }
}
fn command_error(output: &std::process::Output, operation: &str) -> io::Error {
    io::Error::other(format!(
        "{operation}: {}",
        String::from_utf8_lossy(&output.stderr)
    ))
}
fn run(command: &mut Command, operation: &str) -> io::Result<()> {
    let output = command.output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(&output, operation))
    }
}
fn binaries(root: &Path) -> io::Result<PathBuf> {
    if let Some(path) = std::env::var_os("KOMIKA_TEST_POSTGRES_BIN") {
        return validate_bin(PathBuf::from(path));
    }
    for path in [
        "/opt/homebrew/opt/postgresql@18/bin",
        "/usr/local/opt/postgresql@18/bin",
        "/usr/lib/postgresql/18/bin",
    ] {
        if Path::new(path).join("postgres").is_file() {
            return validate_bin(PathBuf::from(path));
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            if path.join("postgres").is_file() {
                if let Ok(path) = validate_bin(path) {
                    return Ok(path);
                }
            }
        }
    }
    // Runtime cache is external to the checkout; first use downloads a checked
    // PostgreSQL 18 archive through the repository's dedicated bootstrap script.
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/postgres-test-runtime.py");
    let output = Command::new("python3")
        .arg(script)
        .arg("--runtime")
        .arg(root.join("runtime"))
        .output()?;
    if !output.status.success() {
        return Err(command_error(
            &output,
            "provision PostgreSQL 18 test runtime",
        ));
    }
    validate_bin(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim(),
    ))
}
fn validate_bin(bin: PathBuf) -> io::Result<PathBuf> {
    let output = Command::new(bin.join("postgres"))
        .arg("--version")
        .output()?;
    if !output.status.success() || !String::from_utf8_lossy(&output.stdout).contains(" 18.") {
        return Err(io::Error::other("the test harness requires PostgreSQL 18"));
    }
    for program in ["initdb", "pg_ctl", "psql"] {
        if !bin.join(program).is_file() {
            return Err(io::Error::other(format!(
                "PostgreSQL test runtime lacks {program}"
            )));
        }
    }
    Ok(bin)
}
struct InitializationGuard {
    root: PathBuf,
    bin: Option<PathBuf>,
    server_start_attempted: bool,
    armed: bool,
}
fn record_cleanup(root: &Path, message: &str) {
    use std::io::Write;
    let diagnostic = root.with_file_name(format!(
        "{}.cleanup.log",
        root.file_name().unwrap().to_string_lossy()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    if let Err(error) = options
        .open(diagnostic)
        .and_then(|mut file| writeln!(file, "{message}"))
    {
        eprintln!("{message}; cleanup diagnostic failed: {error}");
    }
}
fn record_command(root: &Path, operation: &str, result: &io::Result<std::process::Output>) {
    match result {
        Ok(output) => record_cleanup(
            root,
            &format!(
                "Owned PostgreSQL {operation} exit={}\n{}{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            ),
        ),
        Err(error) => record_cleanup(
            root,
            &format!("Owned PostgreSQL {operation} failed: {error}"),
        ),
    }
}
impl Drop for InitializationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.server_start_attempted {
            let Some(bin) = &self.bin else {
                record_cleanup(
                    &self.root,
                    "Owned PostgreSQL shutdown could not be confirmed: runtime unknown; data retained",
                );
                return;
            };
            let stop = Command::new(bin.join("pg_ctl"))
                .arg("-D")
                .arg(self.root.join("data"))
                .args(["stop", "-m", "immediate", "-w", "-t", "10"])
                .output();
            record_command(&self.root, "startup cleanup stop", &stop);
            let stopped = match stop {
                Ok(output) if output.status.success() => true,
                _ => {
                    let status = Command::new(bin.join("pg_ctl"))
                        .arg("-D")
                        .arg(self.root.join("data"))
                        .arg("status")
                        .output();
                    record_command(&self.root, "startup cleanup status", &status);
                    matches!(status, Ok(output) if output.status.code() == Some(3))
                }
            };
            if !stopped && self.root.exists() {
                record_cleanup(
                    &self.root,
                    "Owned PostgreSQL shutdown could not be confirmed; data retained",
                );
                return;
            }
        }
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => record_cleanup(&self.root, "Owned PostgreSQL startup directory removed"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                record_cleanup(
                    &self.root,
                    "Owned PostgreSQL startup directory was already removed",
                );
            }
            Err(error) => record_cleanup(
                &self.root,
                &format!("Owned PostgreSQL startup directory removal failed: {error}"),
            ),
        }
    }
}
impl Cluster {
    fn psql(&self, database: &str, statement: &str) -> io::Result<()> {
        let mut command = Command::new(self.bin.join("psql"));
        // libpq reads PGHOSTADDR/PGOPTIONS and other PG* settings in addition to
        // command arguments. No inherited setting may redirect this endpoint.
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("PG") {
                command.env_remove(name);
            }
        }
        run(
            command
                .args([
                    "-X",
                    "--no-password",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-U",
                    "komika_test",
                    "-d",
                    database,
                    "-c",
                    statement,
                ])
                .arg("-h")
                .arg(&self.root)
                .arg("-p")
                .arg(self.port.to_string())
                .env("PGPASSFILE", self.root.join("absent.pgpass")),
            "execute owned test PostgreSQL command",
        )
    }
    fn start() -> io::Result<Self> {
        #[cfg(unix)]
        let temporary_root = PathBuf::from("/tmp");
        #[cfg(not(unix))]
        let temporary_root = std::env::temp_dir();
        let root = temporary_root.join(format!(
            "komika-pg-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&root)?;
        let mut cleanup = InitializationGuard {
            root: root.clone(),
            bin: None,
            server_start_attempted: false,
            armed: true,
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        }
        let cluster = Self::start_at(root, &mut cleanup)?;
        cleanup.armed = false;
        Ok(cluster)
    }
    fn start_at(root: PathBuf, cleanup: &mut InitializationGuard) -> io::Result<Self> {
        let bin = binaries(&root)?;
        cleanup.bin = Some(bin.clone());
        let data = root.join("data");
        run(
            Command::new(bin.join("initdb")).arg("-D").arg(&data).args([
                "-U",
                "komika_test",
                "--auth-local=trust",
                "--auth-host=reject",
                "--encoding=UTF8",
                "--locale-provider=builtin",
                "--builtin-locale=C.UTF-8",
                "--lc-collate=C",
                "--lc-ctype=C",
            ]),
            "initialize owned test PostgreSQL cluster",
        )?;
        // Unix sockets are namespaced by root, so no shared TCP port is reserved.
        let port = 55432;
        cleanup.server_start_attempted = true;
        run(Command::new(bin.join("pg_ctl")).arg("-D").arg(&data).arg("-l").arg(root.join("server.log"))
            .arg("-o").arg(format!("-p {port} -k '{}' -c listen_addresses='' -c max_connections=512 -c shared_buffers=32MB -c fsync=off -c synchronous_commit=off", root.display()))
            .args(["-w", "start"]), "start owned test PostgreSQL cluster")?;
        let script =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/postgres-test-runtime.py");
        let mut watcher_command = Command::new("python3");
        watcher_command
            .arg(script)
            .arg("--watch")
            .arg(&root)
            .arg("--bin")
            .arg(&bin)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // A foreground terminal Ctrl-C must not interrupt the EOF cleanup
            // watcher together with the Rust test process that owns its pipe.
            watcher_command.process_group(0);
        }
        let mut watcher = watcher_command.spawn()?;
        let watchdog = watcher
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("test watchdog pipe unavailable"))?;
        let mut parsed =
            reqwest::Url::parse(&format!("postgres://komika_test@localhost:{port}/postgres"))
                .unwrap();
        parsed.query_pairs_mut().append_pair(
            "host",
            root.to_str()
                .ok_or_else(|| io::Error::other("test temp path is not UTF8"))?,
        );
        let cluster = Self {
            root,
            bin,
            port,
            template: "komika_test_template".into(),
            url: parsed.to_string(),
            lifecycle: Mutex::new(()),
            _watchdog: Mutex::new(watchdog),
        };
        cluster.psql("postgres", "CREATE DATABASE komika_empty_template TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'")?;
        cluster.psql("postgres", &format!("CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'", cluster.template))?;
        let mut template_url = reqwest::Url::parse(&cluster.url).unwrap();
        template_url.set_path(&format!("/{}", cluster.template));
        let url = template_url.to_string();
        // The independent runtime ends after migration, closing every template
        // connection before CREATE DATABASE ... TEMPLATE clones it.
        std::thread::spawn(move || -> io::Result<()> {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(async {
                    let pool = PgPoolOptions::new()
                        .max_connections(1)
                        .connect_with(PgConnectOptions::from_str(&url).map_err(io::Error::other)?)
                        .await
                        .map_err(io::Error::other)?;
                    let migrated = crate::db::MIGRATOR_PG
                        .run(&pool)
                        .await
                        .map_err(io::Error::other);
                    pool.close().await;
                    migrated
                })
        })
        .join()
        .map_err(|_| io::Error::other("test template migration panicked"))??;
        Ok(cluster)
    }
}
fn cluster() -> io::Result<&'static Cluster> {
    CLUSTER
        .get_or_init(|| Cluster::start().map_err(|error| error.to_string()))
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
}
pub fn clone_database() -> io::Result<DisposableDatabase> {
    clone_from(false)
}
pub fn empty_database() -> io::Result<DisposableDatabase> {
    clone_from(true)
}
fn clone_from(empty: bool) -> io::Result<DisposableDatabase> {
    let cluster = cluster()?;
    let _guard = cluster.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
    let name = format!("komika_app_test_{}", uuid::Uuid::new_v4().simple());
    cluster.psql(
        "postgres",
        &format!(
            "CREATE DATABASE \"{name}\" TEMPLATE {}",
            if empty {
                "komika_empty_template"
            } else {
                &cluster.template
            }
        ),
    )?;
    let mut url = reqwest::Url::parse(&cluster.url).unwrap();
    url.set_path(&format!("/{name}"));
    Ok(DisposableDatabase {
        url: url.to_string(),
        name,
    })
}
