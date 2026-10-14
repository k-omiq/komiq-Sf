use super::*;
use futures::FutureExt;
use std::{borrow::Cow, cell::RefCell, future::Future, pin::Pin};

thread_local! {
    // `#[tokio::test]` runs a current-thread runtime, so one test's lines stay
    // on that test's own thread.
    static LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    // What a test does at the moment a line is said, on the thread that says
    // it: the one place where a test can act between two steps of a stage.
    static ON_LINE: RefCell<Option<Reaction>> = const { RefCell::new(None) };
}
type Reaction = Box<dyn FnMut(&str)>;
pub(super) fn capture(line: &str) {
    LINES.with(|lines| lines.borrow_mut().push(line.to_owned()));
    ON_LINE.with(|reaction| {
        if let Some(reaction) = reaction.borrow_mut().as_mut() {
            reaction(line);
        }
    });
}
/// React to every operator line of this thread from now on; `None` ends it.
fn on_line(reaction: Option<Reaction>) {
    ON_LINE.with(|slot| *slot.borrow_mut() = reaction);
}
/// Operator lines since the last call.
fn said() -> Vec<String> {
    LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()))
}
fn count_lines(lines: &[String], part: &str) -> usize {
    lines.iter().filter(|line| line.contains(part)).count()
}

struct Directory(PathBuf);
impl Directory {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("komika-startup-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self(path)
    }
    fn source(&self) -> PathBuf {
        self.0.join("komika.sqlite3")
    }
}
impl Drop for Directory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn decision_file_missing() {
    assert_eq!(decision(false, Target::Empty, false), Decision::Nothing);
    assert_eq!(decision(false, Target::Unfinished, false), Decision::Refuse);
}
#[test]
fn decision_file_present_empty_postgres() {
    assert_eq!(decision(true, Target::Empty, false), Decision::Import);
}
#[test]
fn decision_file_present_postgres_with_data() {
    assert_eq!(decision(true, Target::Other, false), Decision::Warn);
    assert_eq!(decision(true, Target::Complete, false), Decision::Warn);
}
#[test]
fn decision_crashed_import_state() {
    assert_eq!(decision(true, Target::Unfinished, false), Decision::Import);
    for target in [Target::Empty, Target::Unfinished, Target::Complete] {
        assert_eq!(decision(true, target, true), Decision::Resume);
    }
}
#[test]
fn decision_already_renamed() {
    assert_eq!(decision(false, Target::Complete, false), Decision::Nothing);
    assert_eq!(decision(false, Target::Complete, true), Decision::Resume);
}
#[test]
fn explicit_path_and_covers_neighbor() {
    assert!(source_path(Some("relative.sqlite3".into()), "sqlite::memory:").is_err());
    assert_eq!(source_path(None, "sqlite::memory:").unwrap(), None);
    assert_eq!(
        source_path(None, "sqlite:///tmp/startup-covers/covers.sqlite3").unwrap(),
        Some(PathBuf::from("/tmp/startup-covers/komika.sqlite3"))
    );
}
#[test]
fn disk_budget_includes_staging_migrations_and_repair() {
    let original = BTreeMap::from([
        (
            String::new(),
            Fingerprint {
                bytes: 1024,
                sha256: String::new(),
            },
        ),
        (
            "-wal".into(),
            Fingerprint {
                bytes: 512,
                sha256: String::new(),
            },
        ),
    ]);
    assert_eq!(
        disk_budget(&original).unwrap(),
        8 * 1536 + 256 * 1024 * 1024
    );
    assert!(disk_budget(&BTreeMap::from([(
        String::new(),
        Fingerprint {
            bytes: u64::MAX,
            sha256: String::new()
        }
    )]))
    .is_err());
}
#[test]
fn journal_is_atomic_and_rejects_a_changed_original() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let record = Journal {
        version: 1,
        target: "test".into(),
        original: inventory(&path).unwrap(),
        inventory_ready: true,
        stage: Stage::Copy,
        archive_stamp: None,
        work: None,
        cli_resume: false,
        created: work_suffixes(),
        temporary: format!(".import-work-state.next-{}", uuid::Uuid::new_v4()),
        probes: ["a", "b"]
            .iter()
            .map(|end| format!(".import-probe-{}-{end}", uuid::Uuid::new_v4()))
            .collect(),
        prepared_migrations: migrations_fingerprint(),
        prepared_plan: plan_fingerprint(),
        rows: BTreeMap::new(),
        kept: None,
    };
    let state = sibling(&path, ".import-work-state");
    save(&state, &record).unwrap();
    fs::write(sibling(&state, ".next"), b"partial crash write").unwrap();
    let loaded: Journal = serde_json::from_slice(&fs::read(&state).unwrap()).unwrap();
    assert_eq!(loaded.original, record.original);
    fs::write(&path, b"changed").unwrap();
    assert_ne!(inventory(&path).unwrap(), loaded.original);
}

#[tokio::test]
async fn sqlite_wal_snapshot_preserves_every_original_byte() {
    let dir = Directory::new();
    let path = dir.source();
    // A subprocess exits without close/checkpoint, just like an unclean stop.
    // This test child uses the exact bundled SQLx SQLite engine, not host Python.
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("pg_import::startup::tests::sqlite_wal_fixture_child")
        .arg("--nocapture")
        .env("KOMIKA_STARTUP_WAL_TEST_PATH", &path)
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(86));
    assert!(sibling(&path, "-wal").is_file());
    let original = inventory(&path).unwrap();
    snapshot(&path, &original, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    assert_eq!(inventory(&path).unwrap(), original);
    let work = sibling(&path, ".import-work");
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&work)
        .read_only(true);
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT value FROM wal_row")
            .fetch_one(&mut conn)
            .await
            .unwrap(),
        42
    );
    conn.close().await.unwrap();
    // Copy restart replaces an incomplete/uncheckpointed private snapshot only.
    fs::write(&work, b"partial snapshot").unwrap();
    snapshot(&path, &original, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    assert_eq!(inventory(&path).unwrap(), original);
}
#[tokio::test]
async fn sqlite_wal_fixture_child() {
    let Some(path) = std::env::var_os("KOMIKA_STARTUP_WAL_TEST_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let existing = path.exists();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    let statement = if existing {
        "INSERT INTO users(id,username,email,password_hash,created_at) VALUES('00000000-0000-4000-8000-000000000999','wal-owner','wal-owner@example.invalid','fixture-secret','2020-01-01T00:00:00Z')"
    } else {
        "CREATE TABLE wal_row(value INTEGER); INSERT INTO wal_row VALUES(42)"
    };
    sqlx::raw_sql(statement).execute(&mut conn).await.unwrap();
    std::process::exit(86);
}
#[test]
fn partial_archive_resumes_without_orphan_wal_or_overwrite() {
    let dir = Directory::new();
    let path = dir.source();
    for suffix in ["", "-wal", "-shm"] {
        fs::write(sibling(&path, suffix), suffix.as_bytes()).unwrap();
    }
    let journal = Journal {
        version: 1,
        target: "test".into(),
        original: inventory(&path).unwrap(),
        inventory_ready: true,
        stage: Stage::Rename,
        archive_stamp: Some("20261011T123456Z".into()),
        work: None,
        cli_resume: false,
        created: work_suffixes(),
        temporary: format!(".import-work-state.next-{}", uuid::Uuid::new_v4()),
        probes: ["a", "b"]
            .iter()
            .map(|end| format!(".import-probe-{}-{end}", uuid::Uuid::new_v4()))
            .collect(),
        prepared_migrations: migrations_fingerprint(),
        prepared_plan: plan_fingerprint(),
        rows: BTreeMap::new(),
        kept: None,
    };
    assert!(archive(&path, &journal, Some(Stage::Rename)).is_err());
    assert!(path.exists());
    assert!(!sibling(&path, "-wal").exists());
    archive(&path, &journal, None).unwrap();
    let backup = sibling(&path, ".imported-20261011T123456Z");
    assert!(!path.exists());
    for (suffix, expected) in &journal.original {
        assert_eq!(&fingerprint(&sibling(&backup, suffix)).unwrap(), expected);
    }
    archive(&path, &journal, None).unwrap();
    fs::write(&path, b"new unrelated source").unwrap();
    assert!(archive(&path, &journal, None).is_err());
    assert_eq!(fs::read(&path).unwrap(), b"new unrelated source");
}

async fn golden(path: &Path) {
    let (file, writer) = crate::pg_import::tests::fixture().await;
    writer.close().await.unwrap();
    fs::rename(&file.0, path).unwrap();
}

type Case = fn(PathBuf, String) -> Pin<Box<dyn Future<Output = ()>>>;
fn boxed(future: impl Future<Output = ()> + 'static) -> Pin<Box<dyn Future<Output = ()>>> {
    Box::pin(future)
}
async fn with_postgres(case: Case) {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let admin_url = crate::test_postgres::admin_url().expect("private PostgreSQL 18 cluster");
    let mut admin = crate::pg_import::connect(&admin_url).await.unwrap();
    let name = format!("komika_startup_test_{}", uuid::Uuid::new_v4().simple());
    let quote = crate::pg_import::plan::quote;
    sqlx::query(&format!("CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'", quote(&name))).execute(&mut admin).await.unwrap();
    let mut endpoint = reqwest::Url::parse(&admin_url).unwrap();
    endpoint.set_path(&format!("/{name}"));
    let result = std::panic::AssertUnwindSafe(case(path, endpoint.to_string()))
        .catch_unwind()
        .await;
    sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", quote(&name)))
        .execute(&mut admin)
        .await
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
fn record(path: &Path) -> Journal {
    serde_json::from_slice(&fs::read(sibling(path, ".import-work-state")).unwrap()).unwrap()
}
fn archived(path: &Path) -> PathBuf {
    sibling(
        path,
        &format!(".imported-{}", record(path).archive_stamp.unwrap()),
    )
}

#[tokio::test]
async fn postgres_fresh_import_renames_after_verify_and_second_start_is_noop() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(record(&path).stage, Stage::Done);
            assert!(!path.exists());
            assert!(!sibling(&path, ".import-work").exists());
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            assert!(crate::pg_import::import_complete(&mut target)
                .await
                .unwrap());
            let count: i64 = sqlx::query_scalar("SELECT count(*) FROM chapter")
                .fetch_one(&mut target)
                .await
                .unwrap();
            assert!(count > 0);
            hang_up(target).await;
            let state = fs::read(sibling(&path, ".import-work-state")).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                state
            );
        })
    })
    .await;
}
#[tokio::test]
async fn postgres_old_migration_source_is_upgraded_only_in_work_copy() {
    with_postgres(|path, url| boxed(async move {
        for suffix in COMPONENTS { remove_owned(&sibling(&path, suffix)).unwrap(); }
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).create_if_missing(true).disable_statement_logging();
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        let old = sqlx::migrate::Migrator {
            migrations: Cow::Owned(crate::pg_import::source::SQLITE.iter().filter(|m| m.version <= 112).cloned().collect()),
            ..sqlx::migrate::Migrator::DEFAULT
        };
        old.run(&mut conn).await.unwrap();
        sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('00000000-0000-4000-8000-000000000900','owner','owner@example.invalid','fixture-secret','2020-01-01T00:00:00Z')").execute(&mut conn).await.unwrap();
        conn.close().await.unwrap();
        let original = inventory(&path).unwrap();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        let mut target = crate::pg_import::connect(&url).await.unwrap();
        assert_eq!(sqlx::query_scalar::<_, String>("SELECT username FROM users").fetch_one(&mut target).await.unwrap(), "owner");
    })).await;
}
#[tokio::test]
async fn postgres_kill_and_resume_at_every_stage_boundary() {
    // Each iteration uses a new private database; no skip-return PG tests.
    for stop in [
        Stage::Copy,
        Stage::Migrate,
        Stage::Repair,
        Stage::Import,
        Stage::Verify,
        Stage::Rename,
    ] {
        let dir = Directory::new();
        let path = dir.source();
        golden(&path).await;
        let admin_url = crate::test_postgres::admin_url().unwrap();
        let mut admin = crate::pg_import::connect(&admin_url).await.unwrap();
        let name = format!("komika_startup_test_{}", uuid::Uuid::new_v4().simple());
        let quote = crate::pg_import::plan::quote;
        sqlx::query(&format!("CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'", quote(&name))).execute(&mut admin).await.unwrap();
        let mut endpoint = reqwest::Url::parse(&admin_url).unwrap();
        endpoint.set_path(&format!("/{name}"));
        let url = endpoint.to_string();
        let original = inventory(&path).unwrap();
        let result = std::panic::AssertUnwindSafe(async {
            assert!(run_path(&path, &url, Some(stop))
                .await
                .unwrap_err()
                .to_string()
                .contains("test interruption"));
            assert_eq!(record(&path).stage, stop);
            if stop != Stage::Rename {
                assert_eq!(inventory(&path).unwrap(), original);
            }
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(record(&path).stage, Stage::Done);
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            run_path(&path, &url, None).await.unwrap();
        })
        .catch_unwind()
        .await;
        sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", quote(&name)))
            .execute(&mut admin)
            .await
            .unwrap();
        if let Err(panic) = result {
            std::panic::resume_unwind(panic);
        }
    }
}
#[tokio::test]
async fn postgres_verifier_failure_keeps_original_and_blocks_rename() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(Stage::Import))
                .await
                .unwrap_err();
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            sqlx::query("UPDATE users SET password_hash='changed-fixture' WHERE username='owner'")
                .execute(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            let error = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                error.contains("verify stage") && error.contains("mismatch"),
                "{error}"
            );
            assert!(!error.contains("changed-fixture") && !error.contains("fixture-secret"));
            assert_eq!(inventory(&path).unwrap(), original);
            assert_eq!(record(&path).archive_stamp, None);
        })
    })
    .await;
}
#[tokio::test]
async fn postgres_existing_data_and_completed_cli_import_are_not_overwritten() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            sqlx::raw_sql("CREATE TABLE unrelated(value bigint); INSERT INTO unrelated VALUES(42)")
                .execute(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!sibling(&path, ".import-work-state").exists());
        })
    })
    .await;
    with_postgres(|path, url| {
        boxed(async move {
            crate::pg_import::importer::run(&path, &url, None)
                .await
                .unwrap();
            let original = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!sibling(&path, ".import-work-state").exists());
        })
    })
    .await;
}
#[tokio::test]
async fn postgres_resumes_importer_table_checkpoint_and_refuses_missing_source() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, Some(Stage::Repair))
                .await
                .unwrap_err();
            let work = sibling(&path, ".import-work");
            let mut journal = record(&path);
            journal.stage = Stage::Import;
            journal.work = Some(fingerprint(&work).unwrap());
            save(&sibling(&path, ".import-work-state"), &journal).unwrap();
            assert!(crate::pg_import::importer::run(&work, &url, Some(1))
                .await
                .is_err());
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            assert_eq!(inspect(&mut target).await.unwrap(), Target::Unfinished);
            hang_up(target).await;
            let original = inventory(&path).unwrap();
            let moved = sibling(&path, ".temporarily-missing");
            fs::rename(&path, &moved).unwrap();
            // Fix round 4: still refused, and the refusal names the staged copy
            // and the exact rename that continues the import.
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.starts_with("recovery stage:")
                    && refusal.contains("is missing, and PostgreSQL holds only part of its data")
                    && refusal.contains(&format!(
                        "rename {} to ",
                        sibling(&resolve_source(&path).unwrap(), ".import-work-source").display()
                    ))
                    && refusal.contains("drop it and create it again"),
                "{refusal}"
            );
            assert_eq!(status(&url).await, Target::Unfinished);
            fs::rename(&moved, &path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
        })
    })
    .await;
}

#[tokio::test]
async fn sqlite_insufficient_disk_space_preserves_source() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"untouched source").unwrap();
    let mut size = inventory(&path).unwrap();
    size.get_mut("").unwrap().bytes = u64::MAX / 16;
    let error = snapshot(&path, &size, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("not enough") && error.contains("disk space"),
        "{error}"
    );
    assert_eq!(fs::read(&path).unwrap(), b"untouched source");
    assert!(!sibling(&path, ".import-work").exists());
}

#[tokio::test]
async fn postgres_unclean_wal_source_and_partial_archive_resume() {
    with_postgres(|path, url| {
        boxed(async move {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("pg_import::startup::tests::sqlite_wal_fixture_child")
                .arg("--nocapture")
                .env("KOMIKA_STARTUP_WAL_TEST_PATH", &path)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(86));
            let original = inventory(&path).unwrap();
            assert!(original.contains_key("-wal"));
            run_path(&path, &url, Some(Stage::Rename))
                .await
                .unwrap_err();
            assert!(path.exists());
            assert!(!sibling(&path, "-wal").exists());
            assert_eq!(record(&path).stage, Stage::Rename);
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert!(
                !path.exists()
                    && !sibling(&path, "-wal").exists()
                    && !sibling(&path, "-shm").exists()
            );
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            assert_eq!(
                sqlx::query_scalar::<_, String>(
                    "SELECT username FROM users WHERE username='wal-owner'"
                )
                .fetch_one(&mut target)
                .await
                .unwrap(),
                "wal-owner"
            );
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_uncheckpointed_data_cannot_pose_as_crashed_import() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, Some(Stage::Repair))
                .await
                .unwrap_err();
            let work = sibling(&path, ".import-work");
            assert!(crate::pg_import::importer::run(&work, &url, Some(1))
                .await
                .is_err());
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            // Instance settings are late in the plan, so this is not a done table.
            sqlx::query(
                "INSERT INTO instance_setting(key,value,updated_at) VALUES('foreign-row','foreign-value',TIMESTAMPTZ '2020-01-01T00:00:00Z')",
            )
            .execute(&mut target)
            .await
            .unwrap();
            assert!(inspect(&mut target).await.is_err());
            hang_up(target).await;
            let original = inventory(&path).unwrap();
            assert!(run_path(&path, &url, None).await.is_err());
            assert_eq!(inventory(&path).unwrap(), original);
        })
    })
    .await;
}

#[tokio::test]
async fn sqlite_source_preparation_migrates_repairs_and_pins_work_only() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let original = inventory(&path).unwrap();
    snapshot(&path, &original, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    let work = sibling(&path, ".import-work");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&work).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    migrate(&work).await.unwrap();
    repair(&work).await.unwrap();
    let pinned = fingerprint(&work).unwrap();
    let mut source = crate::pg_import::source::open(&work).await.unwrap();
    let precheck = crate::pg_import::source::precheck(&mut source)
        .await
        .unwrap();
    assert!(precheck.passed());
    source.close().await.unwrap();
    assert_eq!(fingerprint(&work).unwrap(), pinned);
    assert_eq!(inventory(&path).unwrap(), original);
}

#[test]
fn repair_lease_survives_its_original_handle() {
    let dir = Directory::new();
    let path = dir.0.join("repair-lock");
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    owner.lock().unwrap();
    let inherited = owner.try_clone().unwrap();
    drop(owner);
    let next = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    assert!(next.try_lock().is_err());
    drop(inherited);
    next.try_lock().unwrap();
}

fn journal_fixture(path: &Path, stage: Stage) -> Journal {
    Journal {
        version: 1,
        target: "test".into(),
        original: inventory(path).unwrap(),
        inventory_ready: true,
        stage,
        archive_stamp: Some("20261011T123456Z".into()),
        work: None,
        cli_resume: false,
        created: work_suffixes(),
        temporary: format!(".import-work-state.next-{}", uuid::Uuid::new_v4()),
        probes: ["a", "b"]
            .iter()
            .map(|end| format!(".import-probe-{}-{end}", uuid::Uuid::new_v4()))
            .collect(),
        prepared_migrations: migrations_fingerprint(),
        prepared_plan: plan_fingerprint(),
        rows: BTreeMap::new(),
        kept: None,
    }
}

/// What an operator does by hand when a refusal says "delete the work files":
/// every recorded work file goes, the staged copy included.
fn cleanup(path: &Path, journal: &Journal) -> Result<()> {
    discard_work(path, journal, Some(Disposable(())))
}

fn journal_link_preserves(suffix: &str) {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original database").unwrap();
    let protected = if suffix == "covers" {
        dir.0.join("covers.sqlite3")
    } else {
        sibling(&path, suffix)
    };
    if protected != path {
        fs::write(&protected, b"protected bytes").unwrap();
    }
    let before = fs::read(&protected).unwrap();
    let state = sibling(&path, ".import-work-state");
    fs::hard_link(&protected, sibling(&state, ".next")).unwrap();
    save(&state, &journal_fixture(&path, Stage::Copy)).unwrap();
    assert!(sibling(&state, ".next").exists());
    assert_eq!(fs::read(&protected).unwrap(), before);
    assert_eq!(record(&path).stage, Stage::Copy);
}
#[test]
fn journal_temporary_hard_link_to_source_preserves_bytes() {
    journal_link_preserves("");
}
#[test]
fn journal_temporary_hard_link_to_wal_preserves_bytes() {
    journal_link_preserves("-wal");
}
#[test]
fn journal_temporary_hard_link_to_covers_preserves_bytes() {
    journal_link_preserves("covers");
}

#[cfg(unix)]
#[test]
fn journal_temporary_symlink_is_unlinked_without_following() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original database").unwrap();
    let before = inventory(&path).unwrap();
    let state = sibling(&path, ".import-work-state");
    std::os::unix::fs::symlink(&path, sibling(&state, ".next")).unwrap();
    save(&state, &journal_fixture(&path, Stage::Copy)).unwrap();
    assert!(sibling(&state, ".next").exists());
    assert_eq!(inventory(&path).unwrap(), before);
    assert!(fs::symlink_metadata(sibling(&state, ".next"))
        .unwrap()
        .file_type()
        .is_symlink());
}

#[tokio::test]
async fn sqlite_work_and_staging_hard_links_are_unlinked_before_copy() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let before = inventory(&path).unwrap();
    for name in [".import-work", ".import-work-source"] {
        fs::hard_link(&path, sibling(&path, name)).unwrap();
    }
    // Fix round 4: a staged log that has no twin at the original name may be
    // the only copy of committed data. It stops the copy and is left alone.
    let log = sibling(&path, ".import-work-source-wal");
    fs::hard_link(&path, &log).unwrap();
    let journal = journal_fixture(&path, Stage::Copy);
    let refusal = snapshot(&path, &before, &journal)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("it is never overwritten or deleted"),
        "{refusal}"
    );
    assert!(same_file(&path, &log).unwrap());
    assert!(same_file(&path, &sibling(&path, ".import-work-source")).unwrap());
    fs::remove_file(&log).unwrap();
    snapshot(&path, &before, &journal).await.unwrap();
    assert_eq!(inventory(&path).unwrap(), before);
    assert!(!same_file(&path, &sibling(&path, ".import-work")).unwrap());
    assert!(!same_file(&path, &sibling(&path, ".import-work-source")).unwrap());
}

#[test]
fn archive_refuses_destination_created_after_stale_absence_check() {
    let dir = Directory::new();
    let from = dir.source();
    let to = sibling(&from, ".imported-20261011T123456Z");
    fs::write(&from, b"original").unwrap();
    assert!(!to.exists()); // The caller's earlier absence check cannot authorize replacement.
    fs::write(&to, b"existing archive").unwrap();
    assert!(move_no_replace(&from, &to).is_err());
    assert_eq!(fs::read(&to).unwrap(), b"existing archive");
    assert_eq!(fs::read(&from).unwrap(), b"original");
}

#[test]
fn archive_recovers_kill_between_link_and_unlink() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let journal = journal_fixture(&path, Stage::Rename);
    let to = sibling(&path, ".imported-20261011T123456Z");
    fs::hard_link(&path, &to).unwrap();
    archive(&path, &journal, None).unwrap();
    assert!(!path.exists());
    assert_eq!(fs::read(&to).unwrap(), b"original");
}

#[test]
fn journal_save_distinguishes_before_and_after_publication() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let state = sibling(&path, ".import-work-state");
    let mut journal = journal_fixture(&path, Stage::Rename);
    save(&state, &journal).unwrap();
    let next = sibling(&path, &journal.temporary);
    fs::create_dir(&next).unwrap();
    journal.stage = Stage::Done;
    let error = save(&state, &journal).unwrap_err();
    assert!(!error.published);
    assert_eq!(record(&path).stage, Stage::Rename);
    fs::remove_dir(&next).unwrap();
    let error = save_with_sync(&state, &journal, |_| {
        anyhow::bail!("injected directory fsync error")
    })
    .unwrap_err();
    assert!(error.published);
    assert_eq!(record(&path).stage, Stage::Done);
}

#[tokio::test]
async fn invalid_import_path_has_prefix_and_decision_stage() {
    let error = run_configured(
        "unused PostgreSQL URL",
        "sqlite::memory:",
        Some("relative.sqlite3".into()),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.starts_with("sqlite import: decision stage:"),
        "{error}"
    );
    assert!(error.contains("KOMIKA_SQLITE_IMPORT_PATH"));
}

#[tokio::test]
async fn sqlite_snapshot_and_repair_flush_with_write_access() {
    // On Windows this exercises FlushFileBuffers after both operations. The
    // previous read-only File::open(...).sync_all() failed at snapshot completion.
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let before = inventory(&path).unwrap();
    snapshot(&path, &before, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    repair(&sibling(&path, ".import-work")).await.unwrap();
    assert_eq!(inventory(&path).unwrap(), before);
}

#[tokio::test]
async fn postgres_migrated_empty_schema_is_imported() {
    with_postgres(|path, url| {
        boxed(async move {
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            crate::db::MIGRATOR_PG.run(&mut target).await.unwrap();
            assert_eq!(inspect(&mut target).await.unwrap(), Target::Empty);
            hang_up(target).await;
            let before = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(record(&path).stage, Stage::Done);
            assert_eq!(inventory(&archived(&path)).unwrap(), before);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_one_application_row_blocks_startup_and_cli_import() {
    with_postgres(|path, url| {
        boxed(async move {
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            crate::db::MIGRATOR_PG.run(&mut target).await.unwrap();
            sqlx::query("INSERT INTO instance_setting VALUES('normal-start','value',now())")
                .execute(&mut target)
                .await
                .unwrap();
            assert_eq!(inspect(&mut target).await.unwrap(), Target::Other);
            hang_up(target).await;
            let before = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&path).unwrap(), before);
            assert!(!sibling(&path, ".import-work-state").exists());
            assert!(crate::pg_import::importer::run(&path, &url, None)
                .await
                .is_err());
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_file_missing_marks_normal_start_and_prevents_later_import() {
    with_postgres(|path, url| {
        boxed(async move {
            let moved = sibling(&path, ".held");
            fs::rename(&path, &moved).unwrap();
            run_path(&path, &url, None).await.unwrap();
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            assert!(sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('public.komika_server_started') IS NOT NULL"
            )
            .fetch_one(&mut target)
            .await
            .unwrap());
            crate::db::MIGRATOR_PG.run(&mut target).await.unwrap();
            assert_eq!(inspect(&mut target).await.unwrap(), Target::Other);
            hang_up(target).await;
            fs::rename(&moved, &path).unwrap();
            let before = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&path).unwrap(), before);
            assert!(!sibling(&path, ".import-work-state").exists());
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_journal_free_cli_checkpoint_resumes_with_free_pages() {
    with_postgres(|path, url| boxed(async move {
        let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
        let mut source = SqliteConnection::connect_with(&options).await.unwrap();
        sqlx::raw_sql("CREATE TABLE freed(payload BLOB); INSERT INTO freed VALUES(zeroblob(1048576)); DROP TABLE freed;").execute(&mut source).await.unwrap();
        let pages: i64 = sqlx::query_scalar("PRAGMA freelist_count").fetch_one(&mut source).await.unwrap();
        assert!(pages > 0);
        source.close().await.unwrap();
        assert!(crate::pg_import::importer::run(&path, &url, Some(1)).await.is_err());
        assert!(!sibling(&path, ".import-work-state").exists());
        let before = inventory(&path).unwrap();
        run_path(&path, &url, None).await.unwrap();
        assert!(record(&path).cli_resume);
        assert_eq!(inventory(&archived(&path)).unwrap(), before);
    })).await;
}

#[tokio::test]
async fn postgres_connection_spelling_changes_preserve_pending_and_done_import() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, Some(Stage::Repair))
                .await
                .unwrap_err();
            let mut alternate = reqwest::Url::parse(&url).unwrap();
            // The fixture's socket host query remains intact, so both spellings
            // reach the identical private database without a TCP listener.
            alternate.set_host(Some("127.0.0.1")).unwrap();
            alternate
                .query_pairs_mut()
                .append_pair("application_name", "startup-regression");
            alternate
                .set_password(Some("unused-test-password"))
                .unwrap();
            run_path(&path, alternate.as_str(), None).await.unwrap();
            let before = fs::read(sibling(&path, ".import-work-state")).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                before
            );
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_published_done_with_restored_original_finishes_archival() {
    with_postgres(|path, url| {
        boxed(async move {
            let before = inventory(&path).unwrap();
            run_path(&path, &url, Some(Stage::Rename))
                .await
                .unwrap_err();
            let state = sibling(&path, ".import-work-state");
            let mut journal = record(&path);
            journal.stage = Stage::Done;
            assert!(
                save_with_sync(&state, &journal, |_| anyhow::bail!(
                    "injected directory fsync error"
                ))
                .unwrap_err()
                .published
            );
            rollback_archive(&path, &journal).unwrap();
            assert_eq!(inventory(&path).unwrap(), before);
            assert_eq!(record(&path).stage, Stage::Done);
            run_path(&path, &url, None).await.unwrap();
            assert!(!path.exists());
            assert_eq!(inventory(&archived(&path)).unwrap(), before);
            assert_eq!(record(&path).stage, Stage::Done);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_missing_journal_free_unfinished_source_refuses_serving() {
    with_postgres(|path, url| {
        boxed(async move {
            assert!(crate::pg_import::importer::run(&path, &url, Some(1))
                .await
                .is_err());
            fs::rename(&path, sibling(&path, ".held")).unwrap();
            let error = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                error.contains("unfinished import has no SQLite source"),
                "{error}"
            );
            assert!(!sibling(&path, ".import-work-state").exists());
        })
    })
    .await;
}

#[tokio::test]
async fn sqlite_cli_resume_snapshot_keeps_physical_and_data_identity() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(&path);
    let mut writer = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::raw_sql("CREATE TABLE freed(payload BLOB); INSERT INTO freed VALUES(zeroblob(1048576)); DROP TABLE freed;").execute(&mut writer).await.unwrap();
    writer.close().await.unwrap();
    async fn identity(path: &Path) -> String {
        let mut conn = crate::pg_import::source::open(path).await.unwrap();
        let pre = crate::pg_import::source::precheck(&mut conn).await.unwrap();
        let id = crate::pg_import::source::identity(path, &mut conn, &pre)
            .await
            .unwrap();
        conn.close().await.unwrap();
        id
    }
    let pinned = identity(&path).await;
    let before = inventory(&path).unwrap();
    snapshot(&path, &before, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    assert_ne!(identity(&sibling(&path, ".import-work")).await, pinned);
    snapshot_cli(&path, &before, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    assert_eq!(identity(&sibling(&path, ".import-work")).await, pinned);
    assert_eq!(inventory(&path).unwrap(), before);
}

#[test]
fn journal_random_temporary_and_unrecorded_next_are_preserved() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let state = sibling(&path, ".import-work-state");
    let next = sibling(&state, ".next");
    fs::write(&next, b"unrelated sole copy").unwrap();
    let mut journal = journal_fixture(&path, Stage::Copy);
    let other = journal_fixture(&path, Stage::Copy);
    assert_ne!(journal.temporary, other.temporary);
    save(&state, &journal).unwrap();
    // Simulate an interrupted update at this import's recorded random name.
    fs::write(sibling(&path, &journal.temporary), b"torn owned update").unwrap();
    journal.stage = Stage::Migrate;
    save(&state, &journal).unwrap();
    assert_eq!(record(&path).stage, Stage::Migrate);
    cleanup(&path, &journal).unwrap();
    assert_eq!(fs::read(next).unwrap(), b"unrelated sole copy");
    let reserved = sibling(&path, ".import-work");
    fs::write(&reserved, b"unrecorded work").unwrap();
    journal.created.clear();
    assert!(cleanup(&path, &journal)
        .unwrap_err()
        .to_string()
        .contains("unrecorded"));
    assert_eq!(fs::read(reserved).unwrap(), b"unrecorded work");
}

#[cfg(unix)]
#[test]
fn source_directory_rejects_other_owner_and_group_or_other_write() {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let dir = Directory::new();
    let uid = fs::metadata(&dir.0).unwrap().uid();
    validate_directory(&dir.0, uid).unwrap();
    assert!(validate_directory(&dir.0, uid.wrapping_add(1)).is_err());
    for mode in [0o720, 0o702, 0o777] {
        fs::set_permissions(&dir.0, fs::Permissions::from_mode(mode)).unwrap();
        // Fix round 3: resolving the path no longer applies the rule (a start
        // that only warns must not fail on it); the branch that creates files
        // applies it, and the refusal says what to do.
        resolve_source(&dir.source()).unwrap();
        let refusal = require_private_directory(&dir.source())
            .unwrap_err()
            .to_string();
        assert!(
            refusal.contains("not writable by group or others")
                && refusal.contains(&format!("chmod 700 {}", dir.0.display()))
                && refusal.contains("chown")
                && refusal.contains("import-sqlite-to-postgres"),
            "{refusal}"
        );
    }
    fs::set_permissions(&dir.0, fs::Permissions::from_mode(0o700)).unwrap();
    require_private_directory(&dir.source()).unwrap();
    let alias = Directory::new();
    let link = alias.0.join("directory-link");
    std::os::unix::fs::symlink(&dir.0, &link).unwrap();
    assert_eq!(
        resolve_source(&link.join("komika.sqlite3")).unwrap(),
        dir.source().canonicalize().unwrap_or_else(|_| dir
            .0
            .canonicalize()
            .unwrap()
            .join("komika.sqlite3"))
    );
}

#[test]
fn archive_forced_unsupported_rename_falls_back_to_hard_link() {
    let dir = Directory::new();
    let from = dir.source();
    let to = sibling(&from, ".archive");
    fs::write(&from, b"source").unwrap();
    move_with(
        &from,
        &to,
        |_, _| Err(std::io::ErrorKind::Unsupported.into()),
        |a, b| fs::hard_link(a, b),
    )
    .unwrap();
    assert!(!from.exists());
    assert_eq!(fs::read(&to).unwrap(), b"source");
    fs::write(&from, b"other").unwrap();
    assert!(move_with(
        &from,
        &to,
        |_, _| Err(std::io::ErrorKind::Unsupported.into()),
        |a, b| fs::hard_link(a, b)
    )
    .is_err());
    assert_eq!(fs::read(&from).unwrap(), b"other");
}

#[test]
fn archive_capability_probe_cleans_only_recorded_random_files() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let journal = journal_fixture(&path, Stage::Copy);
    save(&sibling(&path, ".import-work-state"), &journal).unwrap();
    let unrelated = sibling(&path, ".import-probe-unrecorded");
    fs::write(&unrelated, b"unrelated").unwrap();
    capability_probe(&path, &journal).unwrap();
    assert!(journal
        .probes
        .iter()
        .all(|name| !sibling(&path, name).exists()));
    assert!(probe_with(&path, &journal, |a, b| move_with(
        a,
        b,
        |_, _| Err(std::io::ErrorKind::Unsupported.into()),
        |_, _| Err(std::io::ErrorKind::Unsupported.into())
    ))
    .is_err());
    assert!(journal
        .probes
        .iter()
        .all(|name| !sibling(&path, name).exists()));
    assert_eq!(fs::read(unrelated).unwrap(), b"unrelated");
}

fn failed_probe(path: &Path, journal: &Journal) -> Result<()> {
    probe_with(path, journal, |a, b| {
        move_with(
            a,
            b,
            |_, _| Err(std::io::ErrorKind::Unsupported.into()),
            |_, _| Err(std::io::ErrorKind::Unsupported.into()),
        )
    })
}

#[tokio::test]
async fn postgres_probe_failure_precedes_every_target_write() {
    with_postgres(|path, url| {
        boxed(async move {
            // Invalid SQLite bytes discriminate probe-before-preflight ordering.
            fs::write(&path, b"not even a SQLite file").unwrap();
            let before = inventory(&path).unwrap();
            let error = run_path_with_probe(&path, &url, None, failed_probe)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("capability probe failed before PostgreSQL writes"),
                "{error}"
            );
            let mut target = super::super::connect(&url).await.unwrap();
            let tables: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pg_tables WHERE schemaname='public'")
                    .fetch_one(&mut target)
                    .await
                    .unwrap();
            assert_eq!(tables, 0);
            assert_eq!(inventory(&path).unwrap(), before);
            assert!(!sibling(&path, ".import-work").exists());
            hang_up(target).await;
            assert!(!record(&path).inventory_ready);
            golden(&path).await;
            // A no-write Copy reservation survives; a supported retry binds it.
            run_path(&path, &url, None).await.unwrap();
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_import_session_limits_override_short_role_defaults() {
    with_postgres(|_, url| boxed(async move {
        let mut target = super::super::connect(&url).await.unwrap();
        let (role, database): (String, String) = sqlx::query_as("SELECT current_user, current_database()").fetch_one(&mut target).await.unwrap();
        for name in ["statement_timeout", "idle_in_transaction_session_timeout", "lock_timeout"] {
            sqlx::query(&format!("ALTER ROLE {} IN DATABASE {} SET {name} = '7ms'", super::super::plan::quote(&role), super::super::plan::quote(&database))).execute(&mut target).await.unwrap();
        }
        hang_up(target).await;
        let mut inherited = PgConnection::connect(&url).await.unwrap();
        let value: String = sqlx::query_scalar("SHOW statement_timeout").fetch_one(&mut inherited).await.unwrap();
        assert_eq!(value, "7ms");
        hang_up(inherited).await;
        let mut target = super::super::connect(&url).await.unwrap();
        let settings: (String, String, String) = sqlx::query_as("SELECT current_setting('statement_timeout'), current_setting('idle_in_transaction_session_timeout'), current_setting('lock_timeout')").fetch_one(&mut target).await.unwrap();
        assert_eq!(settings, ("0".into(), "0".into(), "1min".into()));
        hang_up(target).await;
    })).await;
}

async fn empty_target_after_done(path: &Path, url: &str, recreate: bool) {
    if recreate {
        let admin_url = crate::test_postgres::admin_url().unwrap();
        let mut admin = super::super::connect(&admin_url).await.unwrap();
        let endpoint = reqwest::Url::parse(url).unwrap();
        let name = endpoint.path().trim_start_matches('/');
        let quoted = super::super::plan::quote(name);
        sqlx::query(&format!("DROP DATABASE {quoted} WITH (FORCE)"))
            .execute(&mut admin)
            .await
            .unwrap();
        sqlx::query(&format!("CREATE DATABASE {quoted} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'")).execute(&mut admin).await.unwrap();
        hang_up(admin).await;
    } else {
        let mut target = super::super::connect(url).await.unwrap();
        sqlx::raw_sql("DROP SCHEMA public CASCADE; CREATE SCHEMA public")
            .execute(&mut target)
            .await
            .unwrap();
        // Model an earlier backup after reservation but before completion:
        // a matching UUID alone must never authorize serving an empty target.
        bind_target(&mut target, &record(path)).await.unwrap();
        hang_up(target).await;
    }
    let archive = archived(path);
    let before = inventory(&archive).unwrap();
    // Recorded leftovers must survive this recovery failure, proving no cleanup.
    let leftover = sibling(path, ".import-work");
    fs::write(&leftover, b"recovery work").unwrap();
    let error = run_path(path, url, None).await.unwrap_err().to_string();
    assert!(
        error.contains("recovery stage")
            && error.contains(&archive.display().to_string())
            && error.contains("restore the PostgreSQL backup")
            && error.contains("EMPTY database"),
        "{error}"
    );
    assert_eq!(fs::read(leftover).unwrap(), b"recovery work");
    assert_eq!(inventory(&archive).unwrap(), before);
    let mut target = super::super::connect(url).await.unwrap();
    let marker: bool =
        sqlx::query_scalar("SELECT to_regclass('public.komika_server_started') IS NOT NULL")
            .fetch_one(&mut target)
            .await
            .unwrap();
    assert!(!marker);
    hang_up(target).await;
    // The documented operator action takes the normal fresh-import path.
    for suffix in before.keys() {
        fs::rename(sibling(&archive, suffix), sibling(path, suffix)).unwrap();
    }
    said();
    run_path(path, url, None).await.unwrap();
    // The first line comes before the restored file is hashed.
    let lines = said();
    assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
    assert!(
        lines[1].starts_with("sqlite import: source inventory start: "),
        "{lines:?}"
    );
    assert_eq!(inventory(&archived(path)).unwrap(), before);
}

#[tokio::test]
async fn postgres_done_database_recreated_requires_archive_recovery() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, None).await.unwrap();
            empty_target_after_done(&path, &url, true).await;
        })
    })
    .await;
}
#[tokio::test]
async fn postgres_done_restored_empty_state_requires_archive_recovery() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, None).await.unwrap();
            empty_target_after_done(&path, &url, false).await;
        })
    })
    .await;
}
#[tokio::test]
async fn postgres_done_journal_copied_beside_different_database_refuses() {
    with_postgres(|path, url| boxed(async move {
        run_path(&path, &url, None).await.unwrap();
        let admin_url = crate::test_postgres::admin_url().unwrap();
        let mut admin = super::super::connect(&admin_url).await.unwrap();
        let name = format!("komika_copied_test_{}", uuid::Uuid::new_v4().simple());
        let quoted = super::super::plan::quote(&name);
        sqlx::query(&format!("CREATE DATABASE {quoted} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'")).execute(&mut admin).await.unwrap();
        let mut endpoint = reqwest::Url::parse(&url).unwrap();
        endpoint.set_path(&format!("/{name}"));
        let result = std::panic::AssertUnwindSafe(async {
            let copied = Directory::new();
            let copied_path = copied.source();
            // First make the other database a completed, occupied import with a
            // genuinely different binding, then copy the first directory's record.
            golden(&copied_path).await;
            run_path(&copied_path, endpoint.as_str(), None).await.unwrap();
            assert_ne!(record(&copied_path).target, record(&path).target);
            fs::copy(sibling(&path, ".import-work-state"), sibling(&copied_path, ".import-work-state")).unwrap();
            let archive = archived(&copied_path);
            fs::copy(archived(&path), &archive).unwrap();
            let before = inventory(&archive).unwrap();
            let error = run_path(&copied_path, endpoint.as_str(), None).await.unwrap_err().to_string();
            assert!(error.contains("recovery stage"), "{error}");
            assert_eq!(inventory(&archive).unwrap(), before);
            let mut target = super::super::connect(endpoint.as_str()).await.unwrap();
            assert!(super::super::import_complete(&mut target).await.unwrap());
            assert!(!sqlx::query_scalar::<_, bool>("SELECT to_regclass('public.komika_server_started') IS NOT NULL").fetch_one(&mut target).await.unwrap());
            hang_up(target).await;
            fs::rename(&archive, &copied_path).unwrap();
            assert!(run_path(&copied_path, endpoint.as_str(), None).await.is_err());
            assert_eq!(inventory(&copied_path).unwrap(), before);
        }).catch_unwind().await;
        sqlx::query(&format!("DROP DATABASE {quoted} WITH (FORCE)")).execute(&mut admin).await.unwrap();
        hang_up(admin).await;
        if let Err(panic) = result { std::panic::resume_unwind(panic); }
    })).await;
}

async fn add_newer_migration(path: &Path, wal: bool) -> SqliteConnection {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(if wal {
            sqlx::sqlite::SqliteJournalMode::Wal
        } else {
            sqlx::sqlite::SqliteJournalMode::Delete
        });
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("PRAGMA wal_autocheckpoint=0")
        .execute(&mut conn)
        .await
        .unwrap();
    let latest = super::super::source::SQLITE
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES($1,'future release',1,X'00',0)")
        .bind(latest + 1).execute(&mut conn).await.unwrap();
    conn
}

#[tokio::test]
async fn sqlite_newer_migration_refused_before_copy_in_main_and_committed_wal() {
    for wal in [false, true] {
        let dir = Directory::new();
        let path = dir.source();
        golden(&path).await;
        super::super::preflight::check(&path).unwrap();
        let conn = add_newer_migration(&path, wal).await;
        let before = inventory(&path).unwrap();
        let error = super::super::preflight::check(&path)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("SQLite source has migration")
                && error.contains("this server knows up to")
                && error.contains("newer release"),
            "{error}"
        );
        assert_eq!(inventory(&path).unwrap(), before);
        assert!(!sibling(&path, ".import-work").exists());
        conn.close().await.unwrap();
    }
}

#[tokio::test]
async fn sqlite_migration_error_names_work_copy_and_keeps_sanitized_cause() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    add_newer_migration(&path, false)
        .await
        .close()
        .await
        .unwrap();
    let error = migrate(&path).await.unwrap_err();
    let safe = super::super::cli::safe_error(&error, "");
    assert!(
        safe.contains("SQLite work-copy migration failed")
            && safe.contains("migration")
            && !safe.contains("PostgreSQL migration failed"),
        "{safe}"
    );
    let future = super::super::source::SQLITE
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap()
        + 1;
    assert!(safe.contains(&future.to_string()), "{safe}");
    for message in [
        "SQLite work-copy migration failed: postgres://owner:secret@host/db",
        "SQLite migration failed: password = 'secret with spaces'",
        "SQLite migration failed: password=secret",
    ] {
        let safe = super::super::cli::safe_error(&anyhow::anyhow!("{message}"), "");
        assert!(
            safe.contains("SQLite") && !safe.contains("secret") && !safe.contains("host/db"),
            "{safe}"
        );
    }
}

#[tokio::test]
async fn postgres_newer_source_refuses_in_decision_before_copy_or_binding() {
    with_postgres(|path, url| {
        boxed(async move {
            add_newer_migration(&path, true)
                .await
                .close()
                .await
                .unwrap();
            let before = inventory(&path).unwrap();
            let error = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                error.starts_with("decision stage:") && error.contains("newer release"),
                "{error}"
            );
            let mut target = super::super::connect(&url).await.unwrap();
            assert_eq!(
                sqlx::query_scalar::<_, i64>(
                    "SELECT count(*) FROM pg_tables WHERE schemaname='public'"
                )
                .fetch_one(&mut target)
                .await
                .unwrap(),
                0
            );
            hang_up(target).await;
            assert_eq!(inventory(&path).unwrap(), before);
            assert!(!sibling(&path, ".import-work").exists());
            assert_eq!(record(&path).stage, Stage::Copy);
            assert!(!record(&path).inventory_ready);
        })
    })
    .await;
}

#[tokio::test]
async fn progress_order_fixture_child() {
    let Some(path) = std::env::var_os("KOMIKA_STARTUP_PROGRESS_TEST_PATH") else {
        return;
    };
    let error = run_path(Path::new(&path), "invalid connection URL", None)
        .await
        .unwrap_err();
    eprintln!("fixture connection result: {error}");
    if Path::new(&path).exists() {
        inventory(Path::new(&path)).unwrap();
    }
}

#[test]
fn preparing_progress_precedes_connection_and_inventory_on_fresh_and_resume() {
    let dir = Directory::new();
    let path = dir.source();
    for mode in ["missing", "fresh", "resume"] {
        if mode != "missing" {
            fs::write(&path, b"12345").unwrap();
        }
        if mode == "resume" {
            save(
                &sibling(&path, ".import-work-state"),
                &journal_fixture(&path, Stage::Copy),
            )
            .unwrap();
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("pg_import::startup::tests::progress_order_fixture_child")
            .arg("--nocapture")
            .env("KOMIKA_STARTUP_PROGRESS_TEST_PATH", &path)
            .output()
            .unwrap();
        assert!(output.status.success());
        let stderr = String::from_utf8(output.stderr).unwrap();
        if mode != "resume" {
            // Fix round 3: without a startup record nothing is known before
            // PostgreSQL answers (the start may only warn), so a failed
            // connection prints no progress line. The real fresh-import order
            // is asserted in postgres_progress_lines_only_when_work_is_attempted.
            assert!(!stderr.contains("sqlite import: preparing"), "{stderr}");
        } else {
            let preparing = stderr
                .find("sqlite import: preparing (checking source and target)")
                .unwrap();
            let connection = stderr.find("fixture connection result:").unwrap();
            let inventory = stderr
                .find("sqlite import: source inventory start: 5 bytes")
                .unwrap();
            assert!(preparing < connection && connection < inventory, "{stderr}");
        }
    }
}

#[test]
fn journal_initial_random_collision_refuses_and_unpublished_temp_is_left_alone() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"original").unwrap();
    let state = sibling(&path, ".import-work-state");
    let blocked = journal_fixture(&path, Stage::Copy);
    let unrecorded = sibling(&path, &blocked.temporary);
    fs::write(&unrecorded, b"unrelated or unpublished journal").unwrap();
    let error = save(&state, &blocked).unwrap_err();
    assert!(!error.published && !state.exists());
    assert_eq!(
        fs::read(&unrecorded).unwrap(),
        b"unrelated or unpublished journal"
    );
    let journal = journal_fixture(&path, Stage::Copy);
    save(&state, &journal).unwrap();
    cleanup(&path, &journal).unwrap();
    assert_eq!(
        fs::read(unrecorded).unwrap(),
        b"unrelated or unpublished journal"
    );
}

#[tokio::test]
async fn sqlite_preflight_ignores_uncommitted_future_version_and_torn_wal_tail() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::raw_sql("PRAGMA wal_autocheckpoint=0; PRAGMA cache_size=1; BEGIN")
        .execute(&mut conn)
        .await
        .unwrap();
    let future = super::super::source::SQLITE
        .iter()
        .map(|m| m.version)
        .max()
        .unwrap()
        + 1;
    sqlx::query("INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time) VALUES($1,'uncommitted future',1,X'00',0)").bind(future).execute(&mut conn).await.unwrap();
    sqlx::raw_sql("CREATE TABLE spill(payload BLOB); INSERT INTO spill VALUES(zeroblob(1048576))")
        .execute(&mut conn)
        .await
        .unwrap();
    let wal = sibling(&path, "-wal");
    assert!(fs::metadata(&wal).unwrap().len() > 32);
    let before = inventory(&path).unwrap();
    super::super::preflight::check(&path).unwrap();
    assert_eq!(inventory(&path).unwrap(), before);
    OpenOptions::new()
        .append(true)
        .open(&wal)
        .unwrap()
        .write_all(b"torn tail")
        .unwrap();
    let before = inventory(&path).unwrap();
    super::super::preflight::check(&path).unwrap();
    assert_eq!(inventory(&path).unwrap(), before);
    sqlx::query("ROLLBACK").execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
}

#[test]
fn archive_atomic_rename_succeeds_when_hard_links_are_unavailable() {
    let dir = Directory::new();
    let from = dir.source();
    let to = sibling(&from, ".archive");
    fs::write(&from, b"original").unwrap();
    let link_called = std::cell::Cell::new(false);
    move_with(&from, &to, atomic_no_replace, |_, _| {
        link_called.set(true);
        Err(std::io::ErrorKind::Unsupported.into())
    })
    .unwrap();
    assert!(!link_called.get());
    assert!(!from.exists());
    assert_eq!(fs::read(to).unwrap(), b"original");
}

// ===================== Fix round 3 =====================

async fn with_database<F, Fut>(case: F)
where
    F: FnOnce(PathBuf, String) -> Fut,
    Fut: Future<Output = ()>,
{
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let admin_url = crate::test_postgres::admin_url().expect("private PostgreSQL 18 cluster");
    let mut admin = crate::pg_import::connect(&admin_url).await.unwrap();
    let name = format!("komika_startup_test_{}", uuid::Uuid::new_v4().simple());
    let quote = crate::pg_import::plan::quote;
    sqlx::query(&format!("CREATE DATABASE {} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'", quote(&name))).execute(&mut admin).await.unwrap();
    let mut endpoint = reqwest::Url::parse(&admin_url).unwrap();
    endpoint.set_path(&format!("/{name}"));
    let result = std::panic::AssertUnwindSafe(case(path, endpoint.to_string()))
        .catch_unwind()
        .await;
    sqlx::query(&format!("DROP DATABASE {} WITH (FORCE)", quote(&name)))
        .execute(&mut admin)
        .await
        .unwrap();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

async fn recreate_database(url: &str) {
    let admin_url = crate::test_postgres::admin_url().unwrap();
    let mut admin = super::super::connect(&admin_url).await.unwrap();
    let endpoint = reqwest::Url::parse(url).unwrap();
    let quoted = super::super::plan::quote(endpoint.path().trim_start_matches('/'));
    sqlx::query(&format!("DROP DATABASE {quoted} WITH (FORCE)"))
        .execute(&mut admin)
        .await
        .unwrap();
    sqlx::query(&format!("CREATE DATABASE {quoted} TEMPLATE template0 ENCODING 'UTF8' LOCALE_PROVIDER builtin BUILTIN_LOCALE 'C.UTF-8' LC_COLLATE 'C' LC_CTYPE 'C'")).execute(&mut admin).await.unwrap();
    hang_up(admin).await;
}

/// The previous release was started again (the documented rollback) and wrote
/// to its SQLite database.
async fn previous_release_writes(path: &Path) {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::query("INSERT INTO users(id,username,email,password_hash,created_at) VALUES('00000000-0000-4000-8000-00000000e301','rollback-user','rollback@example.invalid','x','2020-01-01T00:00:00Z')").execute(&mut conn).await.unwrap();
    conn.close().await.unwrap();
}

/// Close a PostgreSQL connection of a test. The server may hang up first, and
/// then the socket shutdown reports "not connected" on macOS; no test is about
/// that.
async fn hang_up(connection: sqlx::PgConnection) {
    let _ = connection.close().await;
}

async fn status(url: &str) -> Target {
    let mut target = super::super::connect(url).await.unwrap();
    let status = inspect(&mut target).await.unwrap();
    hang_up(target).await;
    status
}

async fn usernames(url: &str) -> Vec<String> {
    let mut target = super::super::connect(url).await.unwrap();
    let names = sqlx::query_scalar("SELECT username FROM users ORDER BY 1")
        .fetch_all(&mut target)
        .await
        .unwrap();
    hang_up(target).await;
    names
}

/// This directory's own startup import, killed after the first table reached
/// PostgreSQL: record at Import with the pinned work copy, importer state and
/// one checkpointed table in the database, binding written by this record.
async fn unfinished(path: &Path, url: &str) {
    unfinished_after(path, url, 1).await;
}

async fn unfinished_after(path: &Path, url: &str, tables: usize) {
    run_path(path, url, Some(Stage::Repair)).await.unwrap_err();
    let work = sibling(path, ".import-work");
    let mut journal = record(path);
    journal.stage = Stage::Import;
    journal.work = Some(fingerprint(&work).unwrap());
    save(&sibling(path, ".import-work-state"), &journal).unwrap();
    assert!(crate::pg_import::importer::run(&work, url, Some(tables))
        .await
        .is_err());
    assert_eq!(status(url).await, Target::Unfinished);
}

const RESTARTING: &str = "sqlite import: restarting from the beginning: ";
const PREPARING: &str = "sqlite import: preparing (checking source and target)";

fn leave_warning(path: &Path, archive: &Path) -> String {
    format!(
        "sqlite import: warning: PostgreSQL already holds the imported data; leaving {} in place (earlier archive: {})",
        resolve_source(path).unwrap().display(),
        archive.file_name().unwrap().to_string_lossy()
    )
}

// ---- the restart rule as a pure table ----

#[test]
fn pending_rule_resets_only_our_own_unfinished_import() {
    let stages = [
        Stage::Copy,
        Stage::Migrate,
        Stage::Repair,
        Stage::Import,
        Stage::Verify,
        Stage::Rename,
    ];
    let targets = [
        Target::Empty,
        Target::Unfinished,
        Target::Complete,
        Target::Other,
    ];
    let changes = [
        None,
        Some(Change::Source),
        Some(Change::Binary),
        Some(Change::Plan),
        Some(Change::Work),
        Some(Change::Pinned),
    ];
    for stage in stages {
        for target in targets {
            for owned in [false, true] {
                for cli in [false, true] {
                    for source in [Source::Valid, Source::Missing, Source::Unusable] {
                        for change in changes {
                            for leftovers in [false, true] {
                                let action =
                                    pending(stage, target, owned, cli, source, change, leftovers);
                                let state = format!("{stage:?} {target:?} owned={owned} cli={cli} {source:?} {change:?} leftovers={leftovers} -> {action:?}");
                                let may_reset = stage != Stage::Rename
                                    && target == Target::Unfinished
                                    && owned
                                    && !cli
                                    && source == Source::Valid
                                    && change.is_some();
                                assert_eq!(
                                    matches!(action, Pending::Reset(_)),
                                    may_reset,
                                    "{state}"
                                );
                                // Fix round 4, rule 1: work files are removed and
                                // PostgreSQL is reset only for a VALID file at the
                                // original name. A missing, damaged or unreadable
                                // file never leads to either.
                                if matches!(action, Pending::Restart | Pending::Reset(_)) {
                                    assert_eq!(source, Source::Valid, "{state}");
                                    assert_ne!(stage, Stage::Rename, "{state}");
                                }
                                if action == Pending::Restart {
                                    assert_eq!(target, Target::Empty, "{state}");
                                }
                                // Rule 3: an empty start needs a file that is
                                // really gone, an empty database and no recorded
                                // work file of any kind.
                                assert_eq!(
                                    action == Pending::StartEmpty,
                                    stage != Stage::Rename
                                        && target == Target::Empty
                                        && source == Source::Missing
                                        && !leftovers,
                                    "{state}"
                                );
                                if source != Source::Valid && stage != Stage::Rename {
                                    assert!(
                                        matches!(action, Pending::StartEmpty | Pending::Refuse(_)),
                                        "{state}"
                                    );
                                }
                                // A rename continues only for our completed import.
                                if stage == Stage::Rename {
                                    assert_eq!(
                                        action == Pending::Resume,
                                        target == Target::Complete && owned,
                                        "{state}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn pending_rule_rows() {
    use Pending::*;
    let s = Stage::Import;
    let valid = Source::Valid;
    // a. empty target and a valid file: begin again.
    assert_eq!(
        pending(s, Target::Empty, false, false, valid, None, true),
        Restart
    );
    // c. file gone: an empty start only when no work file is left.
    assert_eq!(
        pending(s, Target::Empty, true, false, Source::Missing, None, false),
        StartEmpty
    );
    assert_eq!(
        pending(s, Target::Empty, true, false, Source::Missing, None, true),
        Refuse(Refusal::SourceGone)
    );
    // A file that cannot be used is never "gone" and never restarted over.
    for target in [Target::Empty, Target::Unfinished, Target::Complete] {
        for leftovers in [false, true] {
            assert_eq!(
                pending(s, target, true, false, Source::Unusable, None, leftovers),
                Refuse(Refusal::SourceUnusable)
            );
        }
    }
    // b. our unfinished import: resume when nothing differs, else reset.
    assert_eq!(
        pending(s, Target::Unfinished, true, false, valid, None, true),
        Resume
    );
    for change in [
        Change::Source,
        Change::Binary,
        Change::Plan,
        Change::Work,
        Change::Pinned,
    ] {
        assert_eq!(
            pending(
                s,
                Target::Unfinished,
                true,
                false,
                valid,
                Some(change),
                true
            ),
            Reset(change)
        );
        assert_eq!(
            pending(s, Target::Unfinished, true, true, valid, Some(change), true),
            Refuse(Refusal::Changed(change))
        );
    }
    assert_eq!(
        pending(
            s,
            Target::Unfinished,
            false,
            false,
            valid,
            Some(Change::Source),
            true
        ),
        Refuse(Refusal::NotOurs)
    );
    assert_eq!(
        pending(
            s,
            Target::Unfinished,
            true,
            false,
            Source::Missing,
            None,
            true
        ),
        Refuse(Refusal::SourceGone)
    );
    // A completed import is never reset.
    assert_eq!(
        pending(
            s,
            Target::Complete,
            true,
            false,
            valid,
            Some(Change::Source),
            true
        ),
        Refuse(Refusal::Changed(Change::Source))
    );
    assert_eq!(
        pending(
            s,
            Target::Complete,
            true,
            false,
            valid,
            Some(Change::Binary),
            true
        ),
        Resume
    );
    assert_eq!(
        pending(s, Target::Complete, false, false, valid, None, true),
        Refuse(Refusal::NotOurs)
    );
    assert_eq!(
        pending(s, Target::Other, true, false, valid, None, true),
        Refuse(Refusal::Unrelated)
    );
    assert_eq!(
        pending(Stage::Rename, Target::Empty, true, false, valid, None, true),
        Refuse(Refusal::RenameWithoutImport)
    );
}

// ---- W1: paths are never turned into URLs ----

#[test]
fn sqlite_paths_reach_options_and_repair_child_unchanged() {
    for text in [
        r"\\?\C:\Users\A B\komika.sqlite3",
        r"C:\Users\é\komika.sqlite3",
        "/data/100% done/k#1?.sqlite3",
        "/data/with space/café 漫画/komika.sqlite3",
        "/data/100%25 done/komika.sqlite3",
        r"\\?\C:\Users\me\AppData\Roaming\app.komika.desktop\komika.sqlite3",
    ] {
        let path = Path::new(text);
        // In-process connections: the path is the option, byte for byte.
        assert_eq!(sqlite_options(path).get_filename(), path);
        let work = sibling(path, ".import-work");
        assert_eq!(sqlite_options(&work).get_filename(), work);
        // The repair child: the production arguments and environment.
        let child = repair_child(&work).unwrap();
        assert_eq!(child.arguments[0], "repair-upload-dates");
        assert_eq!(child.arguments[2], "--apply");
        assert_eq!(child.arguments.len(), 3);
        assert!(
            !child.arguments[1].contains("://"),
            "{}",
            child.arguments[1]
        );
        // catalog/date_repair.rs and db::connect_existing_sqlite both read the
        // argument with exactly this parser.
        let read = sqlx::sqlite::SqliteConnectOptions::from_str(&child.arguments[1]).unwrap();
        assert_eq!(read.get_filename(), work, "{}", child.arguments[1]);
        for name in ["SQLITE_TMPDIR", "TMPDIR", "TMP", "TEMP"] {
            let value = &child
                .environment
                .iter()
                .find(|(key, _)| *key == name)
                .unwrap()
                .1;
            assert_eq!(Path::new(value), work.parent().unwrap());
        }
        assert!(child
            .environment
            .iter()
            .any(|(key, value)| *key == "RUST_LOG" && value == "off"));
    }
}

#[tokio::test]
async fn sqlite_odd_directory_name_survives_snapshot_migrate_and_repair() {
    let dir = Directory::new();
    // Characters that a URL round trip loses or decodes. Windows forbids `?`.
    let odd = dir.0.join(if cfg!(windows) {
        "100%25 done é #1"
    } else {
        "100%25 done é #1?"
    });
    fs::create_dir(&odd).unwrap();
    let path = odd.join("komika.sqlite3");
    golden(&path).await;
    let before = inventory(&path).unwrap();
    snapshot(&path, &before, &journal_fixture(&path, Stage::Copy))
        .await
        .unwrap();
    let work = sibling(&path, ".import-work");
    migrate(&work).await.unwrap();
    // The repair command opens the argument that production builds.
    repair(&work).await.unwrap();
    let mut source = crate::pg_import::source::open(&work).await.unwrap();
    assert!(crate::pg_import::source::precheck(&mut source)
        .await
        .unwrap()
        .passed());
    source.close().await.unwrap();
    assert_eq!(inventory(&path).unwrap(), before);
    let mut names: Vec<String> = fs::read_dir(&odd)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.ends_with("-shm") && !name.ends_with("-wal"))
        .collect();
    names.sort();
    // Nothing was created under a decoded or shortened name.
    assert_eq!(
        names,
        [
            "komika.sqlite3",
            "komika.sqlite3.import-work",
            "komika.sqlite3.import-work-source",
            "komika.sqlite3.import-work.repair-lock"
        ]
    );
    assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
}

// ---- S1: temporary files and free space ----

#[tokio::test]
async fn temporary_directory_fixture_child() {
    let Some(wanted) = std::env::var_os("KOMIKA_STARTUP_TEMP_TEST_DIR") else {
        return;
    };
    let wanted = PathBuf::from(wanted);
    let point = std::env::var_os("KOMIKA_STARTUP_TEMP_TEST_POINT").is_some();
    if point {
        sqlite_temporary_directory(Some(&wanted)).await.unwrap();
    }
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(wanted.parent().unwrap().join("spill.sqlite3"))
        .create_if_missing(true)
        .pragma("temp_store", "FILE")
        .pragma("cache_size", "-64");
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    // A TEMP table and a sort, both far larger than the page cache.
    sqlx::raw_sql("PRAGMA temp.cache_size=-64; CREATE TEMP TABLE spill(payload BLOB); INSERT INTO spill VALUES(zeroblob(8388608))")
        .execute(&mut conn)
        .await
        .unwrap();
    let sorted: i64 = sqlx::query_scalar("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<100000) SELECT count(*) FROM (SELECT randomblob(64) AS r FROM n ORDER BY r)")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    assert_eq!(sorted, 100000);
    conn.close().await.unwrap();
    if point {
        sqlite_temporary_directory(None).await.unwrap();
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        let after: Option<String> = sqlx::query_scalar("PRAGMA temp_store_directory")
            .fetch_optional(&mut conn)
            .await
            .unwrap();
        assert_eq!(after, None);
        conn.close().await.unwrap();
    }
}

#[cfg(unix)]
#[test]
fn sqlite_temporary_files_go_to_the_work_directory_not_to_the_environment() {
    // SQLite unlinks a temporary file as soon as it is open, so the file is
    // seen through its directory's modification time. It is first set to a
    // fixed old value; no clock comparison is involved.
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000_000);
    for point in [false, true] {
        let dir = Directory::new();
        let wanted = dir.0.join("work directory");
        let environment = dir.0.join("environment");
        for directory in [&wanted, &environment] {
            fs::create_dir(directory).unwrap();
            File::open(directory).unwrap().set_modified(old).unwrap();
        }
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("pg_import::startup::tests::temporary_directory_fixture_child")
            .arg("--nocapture")
            .env("KOMIKA_STARTUP_TEMP_TEST_DIR", &wanted)
            .env("SQLITE_TMPDIR", &environment)
            .env("TMPDIR", &environment);
        if point {
            command.env("KOMIKA_STARTUP_TEMP_TEST_POINT", "1");
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let touched =
            |directory: &Path| fs::metadata(directory).unwrap().modified().unwrap() != old;
        // Without the setting SQLite follows SQLITE_TMPDIR (the round-2 space
        // check measured a third place). With it, only the work directory.
        assert_eq!(touched(&wanted), point, "work directory, point={point}");
        assert_eq!(touched(&environment), !point, "environment, point={point}");
    }
}

#[test]
fn df_available_is_found_by_shape_when_names_contain_spaces() {
    let header = "Filesystem 1024-blocks Used Available Capacity Mounted on\n";
    for (line, available) in [
        (
            "/dev/disk3s5 482746452 401627612 33470732 93% /System/Volumes/Data",
            33470732u64,
        ),
        ("map auto_home 0 0 0 100% /System/Volumes/Data/home", 0),
        (
            "//user@nas/My Share 2 1000 400 600 40% /Volumes/My Share 2",
            600,
        ),
        ("overlay   61255492 20000000  38000000      35% /", 38000000),
        ("/dev/mapper/data volume 100 40 60 40% /mnt/100% done", 60),
    ] {
        assert_eq!(
            parse_df_available(&format!("{header}{line}\n")).unwrap(),
            available * 1024,
            "{line}"
        );
    }
    for bad in [
        String::new(),
        header.to_owned(),
        format!("{header}tmpfs - - - - /run\n"),
        // Two places with the shape of the numeric columns: refuse, never guess.
        format!("{header}a 1 2 3 4% 5 6 7 8% /x\n"),
    ] {
        assert!(parse_df_available(&bad).is_err(), "{bad}");
    }
}

#[test]
fn free_space_is_measured_for_a_directory_with_spaces_in_its_name() {
    let dir = Directory::new();
    let spaced = dir.0.join("data volume 100 40 60 40% x");
    fs::create_dir(&spaced).unwrap();
    assert!(free_space(&spaced).unwrap() > 0);
    // One check, on the work directory's own filesystem, names that directory.
    let path = spaced.join("komika.sqlite3");
    fs::write(&path, b"source").unwrap();
    let mut size = inventory(&path).unwrap();
    size.get_mut("").unwrap().bytes = u64::MAX / 16;
    let refusal = require_free_space(&path, &size).unwrap_err().to_string();
    assert!(
        refusal.contains("not enough free disk space in")
            && refusal.contains(&spaced.display().to_string()),
        "{refusal}"
    );
}

// ---- minor items ----

#[test]
fn precheck_failure_lines_name_each_failing_rule() {
    use crate::pg_import::source::{Precheck, Rule};
    let mut precheck = Precheck::default();
    precheck.rules.insert(
        "chapter.published_at: invalid date, clock or offset".into(),
        Rule {
            failures: 3,
            examples: vec!["pk(id) sha256:0123".into()],
            max_bytes: 0,
        },
    );
    precheck.rules.insert(
        "users.users_email_ascii_unique: canonical unique collision".into(),
        Rule {
            failures: 1,
            examples: vec!["2 rows; pk(id) sha256:4567".into()],
            max_bytes: 0,
        },
    );
    precheck
        .rules
        .insert("work: FK orphan".into(), Rule::default());
    assert_eq!(
        crate::pg_import::importer::precheck_failure_lines(&precheck),
        [
            "sqlite import: precheck failed: 2 failing rules",
            "sqlite import: error: precheck rule failed: chapter.published_at: invalid date, clock or offset (3 rows)",
            "sqlite import: error: precheck rule failed: users.users_email_ascii_unique: canonical unique collision (1 rows)",
        ]
    );
}

#[tokio::test]
async fn sqlite_precheck_failure_names_table_and_column_but_no_stored_value() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&path)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::raw_sql("PRAGMA ignore_check_constraints=ON; UPDATE users SET created_at='stored-secret-value' WHERE username='owner'")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    let mut source = crate::pg_import::source::open(&path).await.unwrap();
    let precheck = crate::pg_import::source::precheck(&mut source)
        .await
        .unwrap();
    source.close().await.unwrap();
    assert!(!precheck.passed());
    let lines = crate::pg_import::importer::precheck_failure_lines(&precheck);
    assert!(
        lines.iter().any(|line| line
            .starts_with("sqlite import: error: precheck rule failed: users.created_at: ")
            && line.ends_with(" (1 rows)")),
        "{lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("stored-secret-value") && !line.contains("sha256")),
        "{lines:?}"
    );
}

#[test]
fn verifier_index_expectation_reads_every_migration_statement() {
    use crate::pg_import::verifier::{expected_foreign_keys, expected_indexes, index_statements};
    let mut expected = BTreeMap::new();
    index_statements(
        "CREATE INDEX a ON t(x);\nCREATE UNIQUE INDEX IF NOT EXISTS \"b\"\n    ON t(y);\n    CONSTRAINT \"t_pkey\" PRIMARY KEY (\"id\")\nCREATE INDEX CONCURRENTLY c ON t(z);\n    CONSTRAINT \"t_check\" CHECK (x > 0)",
        &mut expected,
    );
    assert_eq!(
        expected,
        BTreeMap::from([
            ("a".to_owned(), false),
            ("b".to_owned(), true),
            ("c".to_owned(), false),
            ("t_pkey".to_owned(), true),
        ])
    );
    // A later migration that drops or recreates an index moves the expectation.
    index_statements(
        "DROP INDEX IF EXISTS a;\nDROP INDEX c;\nCREATE UNIQUE INDEX c ON t(z);",
        &mut expected,
    );
    assert_eq!(
        expected,
        BTreeMap::from([
            ("b".to_owned(), true),
            ("c".to_owned(), true),
            ("t_pkey".to_owned(), true),
        ])
    );
    let all = expected_indexes();
    assert_eq!(all.get("users_username_ascii_unique"), Some(&true));
    assert_eq!(all.get("idx_work_tag_tag"), Some(&false));
    // Foreign keys come from the plan, never from a number in the verifier.
    let planned: usize = crate::pg_import::plan::tables()
        .iter()
        .map(|table| table.fks.len())
        .sum();
    // A table that only PostgreSQL has adds its own, from the migration that creates it.
    use crate::pg_import::plan::POSTGRES_ONLY;
    use crate::pg_import::verifier::expected_foreign_keys_through;
    let postgres_only: usize = POSTGRES_ONLY.iter().map(|table| table.fks.len()).sum();
    assert_eq!(expected_foreign_keys().len(), planned + 1 + postgres_only);
    assert_eq!(expected_foreign_keys_through(1).len(), planned + 1);
    for table in POSTGRES_ONLY {
        let created = crate::pg_import::importer::POSTGRES
            .iter()
            .any(|m| m.version == table.since && m.sql.contains(table.name));
        assert!(
            created,
            "{} is not created by migration {}",
            table.name, table.since
        );
        assert!(all.contains_key(&format!("{}_pkey", table.name)));
        assert!(crate::pg_import::plan::tables()
            .iter()
            .all(|planned| planned.name != table.name));
    }
}

#[tokio::test]
async fn postgres_migrated_schema_matches_every_index_and_foreign_key_the_verifier_expects() {
    with_postgres(|_, url| {
        boxed(async move {
            const HINT: &str = "a PostgreSQL migration added, dropped or renamed an index or a foreign key that pg_import/verifier.rs does not see: for an index, write the statement on one line starting with CREATE [UNIQUE] INDEX / DROP INDEX or extend index_statements(); for a foreign key, update pg_import/plan.json";
            let mut target = super::super::connect(&url).await.unwrap();
            crate::db::MIGRATOR_PG.run(&mut target).await.unwrap();
            if let Err(error) = super::super::verifier::schema(&mut target).await {
                panic!("{HINT}: {error}");
            }
            if let Err(error) = super::super::verifier::constraints(&mut target).await {
                panic!("{HINT}: {error}");
            }
            // The expectation is derived, not copied from the database: an
            // index or foreign key that no migration file creates is reported.
            sqlx::query("CREATE INDEX idx_from_no_migration ON users(created_at)")
                .execute(&mut target)
                .await
                .unwrap();
            let error = super::super::verifier::schema(&mut target)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("idx_from_no_migration"), "{error}");
            sqlx::query("DROP INDEX idx_from_no_migration")
                .execute(&mut target)
                .await
                .unwrap();
            sqlx::query("ALTER TABLE instance_setting ADD CONSTRAINT fk_from_no_migration FOREIGN KEY (key) REFERENCES instance_setting(key)")
                .execute(&mut target)
                .await
                .unwrap();
            let error = super::super::verifier::constraints(&mut target)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("foreign keys"), "{error}");
            hang_up(target).await;
        })
    })
    .await;
}

#[test]
fn rollback_keeps_archived_sidecars_when_the_original_name_holds_another_file() {
    let dir = Directory::new();
    let path = dir.source();
    for suffix in ["", "-wal"] {
        fs::write(sibling(&path, suffix), format!("original{suffix}")).unwrap();
    }
    let journal = journal_fixture(&path, Stage::Rename);
    // Killed in the middle of the rename: the WAL is archived, main is not.
    assert!(archive(&path, &journal, Some(Stage::Rename)).is_err());
    let archived_wal = sibling(&path, ".imported-20261011T123456Z-wal");
    assert!(archived_wal.exists() && !sibling(&path, "-wal").exists());
    // A previous release ran on the main file meanwhile.
    fs::write(&path, b"changed by a previous release").unwrap();
    assert!(rollback_archive(&path, &journal)
        .unwrap_err()
        .to_string()
        .contains("archived components stay at"));
    // The old WAL was not put beside the changed file, and nothing was lost.
    assert!(!sibling(&path, "-wal").exists());
    assert_eq!(fs::read(&archived_wal).unwrap(), b"original-wal");
    assert_eq!(fs::read(&path).unwrap(), b"changed by a previous release");
    // With the recorded main file the same rollback restores the WAL.
    fs::write(&path, b"original").unwrap();
    rollback_archive(&path, &journal).unwrap();
    assert_eq!(inventory(&path).unwrap(), journal.original);
}

// ---- checker scenarios X1 to X5 with the round-3 outcomes ----

#[tokio::test]
async fn postgres_x1_changed_source_after_failed_copy_restarts_with_the_current_file() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, Some(Stage::Copy)).await.unwrap_err();
            previous_release_writes(&path).await;
            let current = inventory(&path).unwrap();
            said();
            // Round 2: "original SQLite files changed since snapshot" forever.
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
            assert_eq!(inventory(&archived(&path)).unwrap(), current);
            assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
            assert!(!sibling(&path, ".import-work").exists());
            assert!(!sibling(&path, ".import-work-source").exists());
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_x1b_changed_source_with_our_unfinished_import_resets_and_restarts() {
    with_postgres(|path, url| {
        boxed(async move {
            unfinished(&path, &url).await;
            previous_release_writes(&path).await;
            let current = inventory(&path).unwrap();
            said();
            // Round 2: "resume source differs from the pinned source snapshot" forever.
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
            assert!(
                lines.contains(&format!("{RESTARTING}{}", Change::Source.reason())),
                "{lines:?}"
            );
            assert_eq!(inventory(&archived(&path)).unwrap(), current);
            assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
            assert_eq!(status(&url).await, Target::Complete);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_unfinished_import_restarts_when_migrations_plan_or_work_copy_differ() {
    for change in [Change::Binary, Change::Plan, Change::Work] {
        with_database(|path, url| async move {
            unfinished(&path, &url).await;
            let original = inventory(&path).unwrap();
            let state = sibling(&path, ".import-work-state");
            let mut journal = record(&path);
            match change {
                // The work copy was prepared by a binary with other migrations.
                Change::Binary => journal.prepared_migrations = "0".repeat(64),
                Change::Plan => journal.prepared_plan = "0".repeat(64),
                _ => remove_owned(&sibling(&path, ".import-work")).unwrap(),
            }
            save(&state, &journal).unwrap();
            said();
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert!(
                lines.contains(&format!("{RESTARTING}{}", change.reason())),
                "{lines:?}"
            );
            assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert_eq!(record(&path).prepared_migrations, migrations_fingerprint());
            assert_eq!(record(&path).prepared_plan, plan_fingerprint());
        })
        .await;
    }
}

#[tokio::test]
async fn postgres_x2_done_with_archive_renamed_back_after_new_writes_warns_and_starts() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, None).await.unwrap();
            let archive = archived(&path);
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            sqlx::query("UPDATE users SET username='changed-after-cutover' WHERE username='owner'")
                .execute(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            let state = fs::read(sibling(&path, ".import-work-state")).unwrap();
            fs::rename(&archive, &path).unwrap();
            let restored = inventory(&path).unwrap();
            said();
            // Round 2: the Done record was downgraded and every start failed.
            for _ in 0..2 {
                run_path(&path, &url, None).await.unwrap();
                assert_eq!(said(), [leave_warning(&path, &archive)]);
            }
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                state
            );
            assert_eq!(inventory(&path).unwrap(), restored);
            assert!(!archive.exists());
            assert!(usernames(&url)
                .await
                .contains(&"changed-after-cutover".to_owned()));
            // The operator undoes the rename: a silent, normal start again.
            fs::rename(&path, &archive).unwrap();
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), Vec::<String>::new());
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                state
            );
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_x2b_done_with_new_sqlite_file_of_a_previous_release_warns_and_starts() {
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, None).await.unwrap();
            let archive = archived(&path);
            let archived_bytes = inventory(&archive).unwrap();
            let state = fs::read(sibling(&path, ".import-work-state")).unwrap();
            // The previous image started after the cutover and created a database.
            let options = sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .disable_statement_logging();
            let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
            crate::pg_import::source::SQLITE
                .run(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            let created = inventory(&path).unwrap();
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), [leave_warning(&path, &archive)]);
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                state
            );
            assert_eq!(inventory(&path).unwrap(), created);
            assert_eq!(inventory(&archive).unwrap(), archived_bytes);
            assert_eq!(status(&url).await, Target::Complete);
            for suffix in COMPONENTS {
                let _ = fs::remove_file(sibling(&path, suffix));
            }
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), Vec::<String>::new());
            assert_eq!(record(&path).stage, Stage::Done);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_x2c_done_with_identical_original_restored_warns_and_starts() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            let archive = archived(&path);
            fs::rename(&archive, &path).unwrap();
            said();
            // The work copy is gone and cannot be rebuilt to the same bytes
            // (the date repair stamps the current time). Round 2 tried, and
            // failed every start on maintenance_flag.done_at.
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), [leave_warning(&path, &archive)]);
            assert_eq!(record(&path).stage, Stage::Done);
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!sibling(&path, ".import-work").exists());
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_x3_work_copy_prepared_by_an_older_binary_is_rebuilt() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(Stage::Repair))
                .await
                .unwrap_err();
            let work = sibling(&path, ".import-work");
            // The work copy was prepared and pinned by a binary that knew one
            // migration fewer than the binary that now starts.
            let mut conn = SqliteConnection::connect_with(&sqlite_options(&work))
                .await
                .unwrap();
            sqlx::query("DELETE FROM _sqlx_migrations WHERE version=(SELECT max(version) FROM _sqlx_migrations)").execute(&mut conn).await.unwrap();
            sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            let mut journal = record(&path);
            journal.stage = Stage::Import;
            journal.work = Some(fingerprint(&work).unwrap());
            journal.prepared_migrations = "0".repeat(64);
            save(&sibling(&path, ".import-work-state"), &journal).unwrap();
            said();
            // Round 2: "source is not at latest SQLite migration" forever.
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(count_lines(&said(), RESTARTING), 1);
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert_eq!(status(&url).await, Target::Complete);
        })
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn postgres_x4_group_writable_directory_blocks_only_starts_that_create_files() {
    use std::os::unix::fs::PermissionsExt;
    // Done record, original archived, PostgreSQL complete.
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, None).await.unwrap();
            let dir = path.parent().unwrap().to_owned();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
            said();
            let result = run_path(&path, &url, None).await;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            result.unwrap();
            assert_eq!(said(), Vec::<String>::new());
        })
    })
    .await;
    // Warn-only: PostgreSQL complete, SQLite left in place, no record.
    with_postgres(|path, url| {
        boxed(async move {
            crate::pg_import::importer::run(&path, &url, None)
                .await
                .unwrap();
            let dir = path.parent().unwrap().to_owned();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
            said();
            let result = run_path(&path, &url, None).await;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            result.unwrap();
            assert_eq!(said(), ["sqlite import: warning: PostgreSQL already holds data or a completed import; leaving SQLite in place"]);
        })
    })
    .await;
    // A real import in such a directory is still refused, before any file is
    // created and before PostgreSQL is written, with one prefix and the remedy.
    with_postgres(|path, url| {
        boxed(async move {
            let dir = path.parent().unwrap().to_owned();
            let original = inventory(&path).unwrap();
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
            let result = run_path(&path, &url, None).await;
            fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
            let refusal = result.unwrap_err().to_string();
            assert!(
                refusal.starts_with("decision stage: SQLite source directory ")
                    && refusal.contains("chmod 700 ")
                    && !refusal.contains("sqlite import:"),
                "{refusal}"
            );
            assert_eq!(inventory(&path).unwrap(), original);
            assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
            assert_eq!(status(&url).await, Target::Empty);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_x5_pending_record_with_removed_sqlite_file_starts_with_an_empty_database() {
    with_postgres(|path, url| {
        boxed(async move {
            // First attempt refused in the decision stage (source newer than
            // this binary); the operator then removes the file to start fresh.
            add_newer_migration(&path, false)
                .await
                .close()
                .await
                .unwrap();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(refusal.contains("newer release"), "{refusal}");
            for suffix in COMPONENTS {
                let _ = fs::remove_file(sibling(&path, suffix));
            }
            said();
            // Round 2: "original SQLite file is missing" forever.
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), ["sqlite import: warning: the SQLite file named by an unfinished import is gone; starting with an empty database"]);
            assert!(!sibling(&path, ".import-work-state").exists());
            assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 0);
            let mut target = super::super::connect(&url).await.unwrap();
            assert!(sqlx::query_scalar::<_, bool>(
                "SELECT to_regclass('public.komika_server_started') IS NOT NULL"
            )
            .fetch_one(&mut target)
            .await
            .unwrap());
            hang_up(target).await;
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), Vec::<String>::new());
        })
    })
    .await;
    // The same removal after work files exist is refused since fix round 4:
    // see postgres_e1_missing_file_with_a_staged_copy_is_refused.
}

// ---- source or PostgreSQL changes around a kill ----

#[tokio::test]
async fn postgres_source_change_between_kill_and_resume_at_every_stage_boundary() {
    for stop in [
        Stage::Copy,
        Stage::Migrate,
        Stage::Repair,
        Stage::Import,
        Stage::Verify,
        Stage::Rename,
    ] {
        with_database(|path, url| async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(stop)).await.unwrap_err();
            assert_eq!(record(&path).stage, stop);
            if stop == Stage::Rename {
                // The original is archived; the previous release finds no
                // database at its path, gets a copy back and writes to it.
                assert!(!path.exists());
                fs::copy(archived(&path), &path).unwrap();
            }
            previous_release_writes(&path).await;
            let current = inventory(&path).unwrap();
            assert_ne!(current, original);
            let state = sibling(
                resolve_source(&path).unwrap().as_path(),
                ".import-work-state",
            );
            said();
            let result = run_path(&path, &url, None).await;
            let lines = said();
            match stop {
                Stage::Copy | Stage::Migrate | Stage::Repair => {
                    // Nothing reached PostgreSQL: begin again with the current file.
                    result.unwrap();
                    assert_eq!(count_lines(&lines, RESTARTING), 1, "{stop:?} {lines:?}");
                    assert_eq!(inventory(&archived(&path)).unwrap(), current);
                    assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
                }
                Stage::Import | Stage::Verify => {
                    // PostgreSQL holds the completed import of the OLD bytes.
                    // It is never reset and the changed file is never renamed.
                    let refusal = result.unwrap_err().to_string();
                    assert!(
                        refusal.starts_with("recovery stage:")
                            && refusal.contains(&state.display().to_string())
                            && refusal.contains("komika.sqlite3.import-work")
                            && refusal.contains("drop it and create it again")
                            && refusal.contains("To keep what PostgreSQL holds"),
                        "{stop:?} {refusal}"
                    );
                    assert_eq!(count_lines(&lines, RESTARTING), 0, "{lines:?}");
                    assert_eq!(inventory(&path).unwrap(), current);
                    assert_eq!(record(&path).stage, stop);
                    assert_eq!(status(&url).await, Target::Complete);
                    assert!(!usernames(&url).await.contains(&"rollback-user".to_owned()));
                    if stop == Stage::Import {
                        // First manual way out: an empty database imports the current file.
                        recreate_database(&url).await;
                        run_path(&path, &url, None).await.unwrap();
                        assert_eq!(inventory(&archived(&path)).unwrap(), current);
                        assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
                    } else {
                        // Second manual way out: keep PostgreSQL, remove the
                        // record and the work files, move the file away.
                        fs::rename(&path, sibling(&path, ".kept-by-operator")).unwrap();
                        cleanup(&path, &record(&path)).unwrap();
                        fs::remove_file(&state).unwrap();
                        said();
                        run_path(&path, &url, None).await.unwrap();
                        assert_eq!(said(), Vec::<String>::new());
                        assert_eq!(status(&url).await, Target::Complete);
                    }
                }
                Stage::Rename => {
                    let refusal = result.unwrap_err().to_string();
                    assert!(
                        refusal.starts_with("rename stage: SQLite component changed before rename"),
                        "{refusal}"
                    );
                    assert_eq!(inventory(&path).unwrap(), current);
                    assert_eq!(inventory(&archived(&path)).unwrap(), original);
                    assert_eq!(record(&path).stage, Stage::Rename);
                    assert_eq!(status(&url).await, Target::Complete);
                }
                Stage::Done => unreachable!(),
            }
        })
        .await;
    }
}

fn change_source_before_migrate(path: &Path, stage: Stage) {
    if stage == Stage::Migrate {
        // A server of the previous release still runs and writes.
        OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(b"written while the import runs")
            .unwrap();
    }
}

#[tokio::test]
async fn postgres_source_change_between_two_stages_stops_at_the_next_stage() {
    with_postgres(|path, url| {
        boxed(async move {
            let refusal = run_path_with(
                &path,
                &url,
                Faults {
                    before_stage: change_source_before_migrate,
                    ..Faults::NONE
                },
            )
            .await
            .unwrap_err()
            .to_string();
            assert_eq!(
                refusal,
                "migrate stage: original SQLite files changed since snapshot; refusing cutover"
            );
            // Stopped before the work copy was touched again and long before
            // PostgreSQL: the record still waits at Migrate.
            assert_eq!(record(&path).stage, Stage::Migrate);
            assert_eq!(record(&path).archive_stamp, None);
            assert_eq!(status(&url).await, Target::Empty);
            assert!(path.exists());
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_changed_work_copy_blocks_verify_and_rename() {
    for stop in [Stage::Import, Stage::Verify] {
        with_database(|path, url| async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(stop)).await.unwrap_err();
            // A change that the verifier cannot see (no row differs): only the
            // pinned byte hash can refuse this work copy.
            let work = sibling(&path, ".import-work");
            let mut conn = SqliteConnection::connect_with(&sqlite_options(&work))
                .await
                .unwrap();
            sqlx::query("PRAGMA user_version=77")
                .execute(&mut conn)
                .await
                .unwrap();
            conn.close().await.unwrap();
            assert_ne!(Some(fingerprint(&work).unwrap()), record(&path).work);
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.contains("prepared work snapshot changed or is unpinned")
                    && refusal.contains("drop it and create it again"),
                "{refusal}"
            );
            assert_eq!(inventory(&path).unwrap(), original);
            assert_eq!(record(&path).stage, stop);
            assert_eq!(record(&path).archive_stamp, None);
        })
        .await;
    }
}

#[tokio::test]
async fn postgres_changed_after_verify_checkpoint_blocks_rename_and_restores_original() {
    with_postgres(|path, url| {
        boxed(async move {
            // An unclean WAL beside the source makes the rename a two-file move.
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg("pg_import::startup::tests::sqlite_wal_fixture_child")
                .arg("--nocapture")
                .env("KOMIKA_STARTUP_WAL_TEST_PATH", &path)
                .status()
                .unwrap();
            assert_eq!(child.code(), Some(86));
            let original = inventory(&path).unwrap();
            assert!(original.contains_key("-wal"));
            // Verification passed and its checkpoint (Rename, archive stamp) is
            // durable; the process dies after the WAL moved, before main.
            run_path(&path, &url, Some(Stage::Rename))
                .await
                .unwrap_err();
            assert_eq!(record(&path).stage, Stage::Rename);
            assert!(path.exists() && !sibling(&path, "-wal").exists());
            // PostgreSQL changes before the next start.
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            sqlx::query("UPDATE users SET password_hash='changed-fixture' WHERE username='owner'")
                .execute(&mut target)
                .await
                .unwrap();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.starts_with("rename stage:") && refusal.contains("mismatch"),
                "{refusal}"
            );
            assert!(!refusal.contains("changed-fixture") && !refusal.contains("fixture-secret"));
            // Nothing more was renamed, and the WAL is back beside its database.
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!archived(&path).exists());
            assert!(!sibling(&archived(&path), "-wal").exists());
            assert_eq!(record(&path).stage, Stage::Rename);
            // Once PostgreSQL matches again the same record finishes.
            sqlx::query("UPDATE users SET password_hash='fixture-secret' WHERE username='owner'")
                .execute(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert!(!path.exists());
        })
    })
    .await;
}

fn block_done_record(path: &Path, stage: Stage) {
    if stage == Stage::Rename {
        // The next record update cannot be written: its recorded temporary
        // name is a directory.
        fs::create_dir(sibling(path, &record(path).temporary)).unwrap();
    }
}

#[tokio::test]
async fn postgres_failed_done_save_restores_the_original() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            let refusal = run_path_with(
                &path,
                &url,
                Faults {
                    before_stage: block_done_record,
                    ..Faults::NONE
                },
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(refusal.starts_with("rename stage:"), "{refusal}");
            // The rename happened and was undone: Rename intent, original back.
            assert_eq!(record(&path).stage, Stage::Rename);
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!archived(&path).exists());
            assert!(sibling(&path, ".import-work").exists());
            fs::remove_dir(sibling(&path, &record(&path).temporary)).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(record(&path).stage, Stage::Done);
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_interrupted_archive_with_changed_postgres_warns_and_keeps_the_file() {
    with_postgres(|path, url| {
        boxed(async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(Stage::Rename))
                .await
                .unwrap_err();
            // Done was published, its directory sync failed, the rename was undone.
            let state = sibling(&path, ".import-work-state");
            let mut journal = record(&path);
            journal.stage = Stage::Done;
            assert!(
                save_with_sync(&state, &journal, |_| anyhow::bail!("injected"))
                    .unwrap_err()
                    .published
            );
            rollback_archive(&path, &journal).unwrap();
            let archive = archived(&path);
            let mut target = crate::pg_import::connect(&url).await.unwrap();
            sqlx::query("UPDATE users SET username='changed-after-cutover' WHERE username='owner'")
                .execute(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            let done = fs::read(&state).unwrap();
            said();
            // Verification does not pass in this process: nothing is renamed,
            // the Done record stays Done, and the server starts.
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
            assert_eq!(
                count_lines(
                    &lines,
                    "sqlite import: warning: could not finish the earlier archive rename: "
                ),
                1,
                "{lines:?}"
            );
            assert_eq!(lines.last(), Some(&leave_warning(&path, &archive)));
            assert_eq!(count_lines(&lines, "sqlite import: renamed original"), 0);
            // Nothing was finished, so nothing is called complete.
            assert_eq!(count_lines(&lines, "sqlite import: complete: "), 0);
            assert_eq!(inventory(&path).unwrap(), original);
            assert!(!archive.exists());
            assert_eq!(fs::read(&state).unwrap(), done);
            // The work copy is gone, so later starts do not verify again.
            assert!(!sibling(&path, ".import-work").exists());
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), [leave_warning(&path, &archive)]);
        })
    })
    .await;
}

// ---- nobody else's database is ever reset ----

#[tokio::test]
async fn postgres_reset_refuses_every_target_that_is_not_our_unfinished_import() {
    // Every application row, and whether the importer state exists.
    async fn rows(url: &str) -> (i64, bool) {
        let mut target = super::super::connect(url).await.unwrap();
        let mut total = 0;
        for table in crate::pg_import::plan::tables() {
            total += sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM public.{}",
                crate::pg_import::plan::quote(&table.name)
            ))
            .fetch_one(&mut target)
            .await
            .unwrap();
        }
        let state =
            sqlx::query_scalar("SELECT to_regclass('public.komika_import_state') IS NOT NULL")
                .fetch_one(&mut target)
                .await
                .unwrap();
        hang_up(target).await;
        (total, state)
    }
    // A completed import, even our own.
    with_postgres(|path, url| {
        boxed(async move {
            run_path(&path, &url, Some(Stage::Import))
                .await
                .unwrap_err();
            assert_eq!(status(&url).await, Target::Complete);
            let before = rows(&url).await;
            let mut target = super::super::connect(&url).await.unwrap();
            let refusal = reset_target(&mut target, &record(&path))
                .await
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("already complete"), "{refusal}");
            hang_up(target).await;
            assert_eq!(rows(&url).await, before);
            assert!(before.0 > 0);
        })
    })
    .await;
    with_postgres(|path, url| {
        boxed(async move {
            unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
            let before = rows(&url).await;
            let mut target = super::super::connect(&url).await.unwrap();
            // Another startup record (another data directory).
            let mut other = record(&path);
            other.target = uuid::Uuid::new_v4().to_string();
            let refusal = reset_target(&mut target, &other)
                .await
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("belongs to another startup record"), "{refusal}");
            // A command-line resume record, whatever identity it carries.
            let mut cli = record(&path);
            cli.cli_resume = true;
            assert!(reset_target(&mut target, &cli).await.is_err());
            assert_eq!(rows(&url).await, before);
            // One row that no import checkpoint accounts for.
            sqlx::query("INSERT INTO instance_setting(key,value,updated_at) VALUES('foreign-row','foreign-value',TIMESTAMPTZ '2020-01-01T00:00:00Z')")
                .execute(&mut target)
                .await
                .unwrap();
            let refusal = reset_target(&mut target, &record(&path))
                .await
                .unwrap_err()
                .to_string();
            assert!(refusal.contains("unowned or changed rows"), "{refusal}");
            assert_eq!(rows(&url).await, (before.0 + 1, true));
            // A normal server start on this database.
            sqlx::raw_sql("DELETE FROM instance_setting WHERE key='foreign-row'; CREATE TABLE public.komika_server_started (singleton boolean PRIMARY KEY CHECK(singleton))")
                .execute(&mut target)
                .await
                .unwrap();
            assert!(reset_target(&mut target, &record(&path)).await.is_err());
            assert_eq!(rows(&url).await, before);
            assert!(before.0 > 0 && before.1);
            // Our own unfinished import, and nothing else: the migrated schema
            // with zero rows and normal triggers is left, the binding is kept.
            sqlx::query("DROP TABLE public.komika_server_started")
                .execute(&mut target)
                .await
                .unwrap();
            reset_target(&mut target, &record(&path)).await.unwrap();
            assert_eq!(inspect(&mut target).await.unwrap(), Target::Empty);
            assert_eq!(
                startup_binding(&mut target).await.unwrap(),
                Some(record(&path).target)
            );
            let disabled: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid=t.tgrelid JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='public' AND NOT t.tgisinternal AND t.tgenabled <> 'O'")
                .fetch_one(&mut target)
                .await
                .unwrap();
            assert_eq!(disabled, 0);
            hang_up(target).await;
            assert_eq!(rows(&url).await, (0, false));
            // A second reset finds nothing to reset.
            let mut target = super::super::connect(&url).await.unwrap();
            assert!(reset_target(&mut target, &record(&path)).await.is_err());
            hang_up(target).await;
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_second_server_is_refused_while_another_importer_holds_the_target() {
    with_postgres(|path, url| {
        boxed(async move {
            unfinished(&path, &url).await;
            previous_release_writes(&path).await;
            let current = inventory(&path).unwrap();
            let state = fs::read(sibling(&path, ".import-work-state")).unwrap();
            // The other server is in the middle of its own decision or reset.
            let mut first = super::super::connect(&url).await.unwrap();
            assert!(
                sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(192837465,180002)")
                    .fetch_one(&mut first)
                    .await
                    .unwrap()
            );
            said();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert_eq!(refusal, "decision stage: another importer owns the target");
            assert_eq!(count_lines(&said(), RESTARTING), 0);
            assert_eq!(
                fs::read(sibling(&path, ".import-work-state")).unwrap(),
                state
            );
            assert_eq!(inventory(&path).unwrap(), current);
            assert!(sibling(&path, ".import-work").exists());
            // The other server is done (a hang-up alone frees the lock late).
            sqlx::query("SELECT pg_advisory_unlock(192837465,180002)")
                .execute(&mut first)
                .await
                .unwrap();
            hang_up(first).await;
            assert_eq!(status(&url).await, Target::Unfinished);
            // Alone again, the one remaining server does its one checked restart.
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(count_lines(&said(), RESTARTING), 1);
            assert_eq!(inventory(&archived(&path)).unwrap(), current);
        })
    })
    .await;
}

#[tokio::test]
async fn postgres_unfinished_import_of_another_directory_is_never_adopted_or_reset() {
    with_postgres(|path, url| {
        boxed(async move {
            // Directory A reserved the empty database and stopped before importing.
            run_path(&path, &url, Some(Stage::Repair))
                .await
                .unwrap_err();
            let a = record(&path).target;
            // Directory B points at the same database. The database is still
            // empty, so B may take it over, with its own binding.
            let other = Directory::new();
            let b_path = other.source();
            golden(&b_path).await;
            unfinished(&b_path, &url).await;
            let b = record(&b_path).target;
            assert_ne!(a, b);
            let mut target = super::super::connect(&url).await.unwrap();
            assert_eq!(startup_binding(&mut target).await.unwrap(), Some(b.clone()));
            let loaded: i64 = sqlx::query_scalar("SELECT count(*) FROM public.komika_import_table")
                .fetch_one(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            assert_eq!(loaded, 1);
            // A starts again, unchanged and then with a changed source: B's
            // unfinished import is refused, never resumed and never reset.
            let a_state = sibling(
                resolve_source(&path).unwrap().as_path(),
                ".import-work-state",
            );
            for changed in [false, true] {
                if changed {
                    previous_release_writes(&path).await;
                }
                let before = inventory(&path).unwrap();
                said();
                let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
                assert!(
                    refusal.starts_with("recovery stage:")
                        && refusal.contains("did not start")
                        && refusal.contains(&a_state.display().to_string()),
                    "{refusal}"
                );
                assert_eq!(count_lines(&said(), RESTARTING), 0);
                assert_eq!(inventory(&path).unwrap(), before);
                assert_eq!(status(&url).await, Target::Unfinished);
            }
            // Without its record A has even less of a claim.
            let work = record(&path);
            fs::remove_file(&a_state).unwrap();
            cleanup(&path, &work).unwrap();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.contains("holds an unfinished import")
                    && refusal.contains("is missing here"),
                "{refusal}"
            );
            assert!(!a_state.exists());
            let mut target = super::super::connect(&url).await.unwrap();
            assert_eq!(startup_binding(&mut target).await.unwrap(), Some(b));
            let still: i64 = sqlx::query_scalar("SELECT count(*) FROM public.komika_import_table")
                .fetch_one(&mut target)
                .await
                .unwrap();
            hang_up(target).await;
            assert_eq!(still, 1);
            // B itself continues from its checkpoint.
            let b_original = inventory(&b_path).unwrap();
            run_path(&b_path, &url, None).await.unwrap();
            assert_eq!(inventory(&archived(&b_path)).unwrap(), b_original);
        })
    })
    .await;
}

// ---- operator lines ----

#[tokio::test]
async fn postgres_progress_lines_only_when_work_is_attempted() {
    with_postgres(|path, url| {
        boxed(async move {
            said();
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            let place = |part: &str| {
                lines
                    .iter()
                    .position(|line| line.starts_with(part))
                    .unwrap_or_else(|| panic!("{part} missing in {lines:?}"))
            };
            // Fresh import: decided first, then announced before any source
            // byte is hashed and before the first stage.
            assert_eq!(place(PREPARING), 0);
            assert_eq!(count_lines(&lines, PREPARING), 1);
            assert!(
                place("sqlite import: source inventory start: ")
                    < place("sqlite import: disk space: ")
            );
            let order = [
                "sqlite import: copy start",
                "sqlite import: copy complete: ",
                "sqlite import: migrate start",
                "sqlite import: migrate complete: ",
                "sqlite import: repair start",
                "sqlite import: repair complete: ",
                "sqlite import: import start",
                "sqlite import: import complete: ",
                "sqlite import: verify start",
                "sqlite import: verifier PASS: ",
                "sqlite import: verify complete: ",
                "sqlite import: rename start",
                "sqlite import: renamed original to ",
                "sqlite import: rename complete: ",
                "sqlite import: complete: ",
            ];
            for pair in order.windows(2) {
                assert!(place(pair[0]) < place(pair[1]), "{pair:?} in {lines:?}");
            }
            assert_eq!(place(order[14]), lines.len() - 1);
            assert_eq!(count_lines(&lines, RESTARTING), 0);
            assert_eq!(count_lines(&lines, "warning"), 0, "{lines:?}");
            // Done record and no file at the original path: nothing at all.
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), Vec::<String>::new());
        })
    })
    .await;
    // PostgreSQL already holds a completed import: one warning, no progress line.
    with_postgres(|path, url| {
        boxed(async move {
            crate::pg_import::importer::run(&path, &url, None)
                .await
                .unwrap();
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), ["sqlite import: warning: PostgreSQL already holds data or a completed import; leaving SQLite in place"]);
        })
    })
    .await;
    // A resume announces before it connects (the record alone proves work).
    with_postgres(|path, url| {
        boxed(async move {
            unfinished(&path, &url).await;
            said();
            run_path(&path, "invalid connection URL", None)
                .await
                .unwrap_err();
            assert_eq!(said(), [PREPARING]);
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
            assert_eq!(count_lines(&lines, PREPARING), 1);
            assert_eq!(
                count_lines(&lines, "sqlite import: copy start"),
                0,
                "{lines:?}"
            );
            assert_eq!(count_lines(&lines, "sqlite import: import start"), 1);
        })
    })
    .await;
}

// ===================== Fix round 4 =====================
//
// The rule of this round: the importer never deletes a file that may be the
// last good copy of the owner's data.

const KEPT: &str = "sqlite import: warning: the copy of the SQLite database that an earlier import attempt made differs from the file now at ";
const SMALLER: &str =
    "sqlite import: warning: the database being imported is smaller than the copy kept at ";

/// The directory as an owner sees it: every name with what it holds.
fn listing(path: &Path) -> BTreeMap<String, String> {
    let directory = path.parent().unwrap();
    fs::read_dir(directory)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let kind = fs::symlink_metadata(entry.path()).unwrap().file_type();
            let holds = if kind.is_symlink() {
                "link".to_owned()
            } else if kind.is_dir() {
                "directory".to_owned()
            } else {
                fingerprint(&entry.path())
                    .map(|file| format!("{} {}", file.bytes, file.sha256))
                    .unwrap_or_else(|_| "unreadable".to_owned())
            };
            (entry.file_name().to_string_lossy().into_owned(), holds)
        })
        .collect()
}

/// SHA-256 of every regular file in the directory.
fn hashes(path: &Path) -> std::collections::BTreeSet<String> {
    listing(path)
        .into_values()
        .filter_map(|holds| holds.split_once(' ').map(|(_, hash)| hash.to_owned()))
        .collect()
}

/// The owner's database bytes: the components at the original name, the data
/// components of the staged copy, and every file of every kept set. Nothing
/// of this may ever be lost.
fn owner_bytes(path: &Path) -> std::collections::BTreeSet<String> {
    // Kept files are found by a plain directory listing and their name, not
    // by the product's own search: the property must also see a kept file
    // that the product overlooks.
    let directory = path.parent().unwrap();
    let mut files = fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".import-kept-")
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    for base in [path.to_owned(), sibling(path, ".import-work-source")] {
        for suffix in DATA_COMPONENTS {
            files.push(sibling(&base, suffix));
        }
    }
    let mut found = std::collections::BTreeSet::new();
    for file in files {
        // A file of no bytes holds no bytes of the owner's database (SQLite
        // leaves an empty `-wal` beside a copy that it only read).
        if fs::symlink_metadata(&file).is_ok_and(|m| m.file_type().is_file() && m.len() > 0) {
            if let Ok(print) = fingerprint(&file) {
                found.insert(print.sha256);
            }
        }
    }
    found
}

fn kept_files(path: &Path) -> Vec<PathBuf> {
    kept_copies(&resolve_source(path).unwrap())
        .into_iter()
        .map(PathBuf::from)
        .collect()
}

fn staged_file(path: &Path) -> PathBuf {
    sibling(&resolve_source(path).unwrap(), ".import-work-source")
}

async fn row_total(url: &str) -> i64 {
    let mut target = super::super::connect(url).await.unwrap();
    let mut total = 0;
    for table in crate::pg_import::plan::tables() {
        let exists: bool = sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
            .bind(format!(
                "public.{}",
                crate::pg_import::plan::quote(&table.name)
            ))
            .fetch_one(&mut target)
            .await
            .unwrap();
        if exists {
            total += sqlx::query_scalar::<_, i64>(&format!(
                "SELECT count(*) FROM public.{}",
                crate::pg_import::plan::quote(&table.name)
            ))
            .fetch_one(&mut target)
            .await
            .unwrap();
        }
    }
    hang_up(target).await;
    total
}

/// The marker that permanently turns the import off for a database.
async fn started_marker(url: &str) -> bool {
    let mut target = super::super::connect(url).await.unwrap();
    let marker =
        sqlx::query_scalar("SELECT to_regclass('public.komika_server_started') IS NOT NULL")
            .fetch_one(&mut target)
            .await
            .unwrap();
    hang_up(target).await;
    marker
}

fn remove_source(path: &Path) {
    for suffix in COMPONENTS {
        let _ = fs::remove_file(sibling(path, suffix));
    }
}

/// Truncated by a full disk, a bad copy: 4096 zero bytes at the original name.
fn damage_source(path: &Path) {
    for suffix in &COMPONENTS[1..] {
        let _ = fs::remove_file(sibling(path, suffix));
    }
    fs::write(path, vec![0u8; 4096]).unwrap();
}

/// A new, empty database as the previous release creates it when it finds no
/// file at its path.
async fn previous_release_creates_empty_database(path: &Path) {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    super::super::source::SQLITE.run(&mut conn).await.unwrap();
    conn.close().await.unwrap();
}

/// A refused start must leave the directory and PostgreSQL exactly as they
/// were and must never turn the import off.
struct Before {
    files: BTreeMap<String, String>,
    status: Target,
    rows: i64,
}
async fn before(path: &Path, url: &str) -> Before {
    Before {
        files: listing(path),
        status: status(url).await,
        rows: row_total(url).await,
    }
}
async fn assert_nothing_changed(path: &Path, url: &str, was: &Before, what: &str) {
    assert_eq!(listing(path), was.files, "{what}: files");
    assert_data_unchanged(path, url, was, what).await;
}
/// The same without the record file: a resume that passed a stage boundary
/// before it was refused has written that boundary down.
async fn assert_data_unchanged(path: &Path, url: &str, was: &Before, what: &str) {
    let without_record = |mut files: BTreeMap<String, String>| {
        files.retain(|name, _| !name.ends_with(".import-work-state"));
        files
    };
    assert_eq!(
        without_record(listing(path)),
        without_record(was.files.clone()),
        "{what}: files"
    );
    assert_eq!(status(url).await, was.status, "{what}: PostgreSQL state");
    assert_eq!(row_total(url).await, was.rows, "{what}: PostgreSQL rows");
    assert!(!started_marker(url).await, "{what}: normal-start marker");
}

// ---- E1 to E12 of the fourth checker, with the outcomes of this round ----

#[tokio::test]
async fn postgres_e1_missing_file_with_a_staged_copy_is_refused() {
    // Way out 1: rename the staged copy back.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        let staged = staged_file(&path);
        assert_eq!(&fingerprint(&staged).unwrap(), original.get("").unwrap());
        remove_source(&path);
        let was = before(&path, &url).await;
        for attempt in 0..2 {
            said();
            // Round 3 deleted both copies here, started empty and wrote the
            // marker that turns the import off for good.
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            let source = resolve_source(&path).unwrap();
            assert!(
                refusal.starts_with("recovery stage: the SQLite file ")
                    && refusal.contains("named by an unfinished import is missing")
                    && refusal.contains("Nothing was changed")
                    && refusal.contains(&format!(
                        "An exact copy of the database as it was when the import began is at {}.",
                        staged.display()
                    ))
                    && refusal.contains(&format!(
                        "To continue, rename {} to {} and start again",
                        staged.display(),
                        source.display()
                    ))
                    && refusal.contains("to start with an empty database, delete ")
                    && refusal
                        .contains(&sibling(&source, ".import-work-state").display().to_string()),
                "{attempt}: {refusal}"
            );
            let lines = said();
            assert_eq!(count_lines(&lines, "starting with an empty database"), 0);
            assert_eq!(count_lines(&lines, RESTARTING), 0);
            // The staged copy is hashed for this refusal: the first line and
            // then a line that a supervisor reads as work in progress, with
            // the bytes that are hashed.
            assert_eq!(
                lines,
                [
                    PREPARING.to_owned(),
                    format!(
                        "sqlite import: source inventory start: {} bytes",
                        fs::metadata(&staged).unwrap().len()
                    )
                ]
            );
            assert_nothing_changed(&path, &url, &was, "missing file").await;
        }
        fs::rename(&staged, &path).unwrap();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
    // Way out 2: delete the named files and the record, start empty.
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        remove_source(&path);
        run_path(&path, &url, None).await.unwrap_err();
        let state = sibling(&resolve_source(&path).unwrap(), ".import-work-state");
        cleanup(&path, &record(&path)).unwrap();
        fs::remove_file(&state).unwrap();
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(said(), Vec::<String>::new());
        assert!(started_marker(&url).await);
    })
    .await;
    // The owner deletes only the record: the copies have no record now, and
    // the server still does not start empty beside them.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        remove_source(&path);
        let source = resolve_source(&path).unwrap();
        fs::remove_file(sibling(&source, ".import-work-state")).unwrap();
        let staged = staged_file(&path);
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("decision stage: the SQLite file ")
                && refusal.contains("is missing, and files of an earlier import attempt are here without their record: ")
                && refusal.contains("Nothing was changed")
                && refusal.contains("it may be the only one")
                && refusal.contains(&format!(
                    "To import it, rename {} to {}, delete {}, {} and start again",
                    staged.display(),
                    source.display(),
                    sibling(&source, ".import-work").display(),
                    sibling(&source, ".import-work.repair-lock").display()
                ))
                && refusal.contains("move the listed files out of the data directory"),
            "{refusal}"
        );
        assert_eq!(said(), Vec::<String>::new());
        assert_nothing_changed(&path, &url, &was, "copies without a record").await;
        // The first way out, as written: the rename, and every file that it
        // says to delete (round 5 did not name the lock file, and the next
        // start was refused beside it).
        follow_renames(&refusal, "To import it, ");
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
    // A database that already holds data starts, and says what it leaves.
    with_database(|path, url| async move {
        run_path(&path, &url, None).await.unwrap();
        let staged = staged_file(&path);
        fs::write(&staged, b"left by hand").unwrap();
        fs::remove_file(sibling(&path, ".import-work-state")).unwrap();
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(
            said(),
            [format!(
                "sqlite import: warning: unrecorded file {}; leaving it alone",
                staged.display()
            )]
        );
        assert_eq!(fs::read(&staged).unwrap(), b"left by hand");
    })
    .await;
    // Only the derived work copy is left: still never an empty start.
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        remove_source(&path);
        remove_owned(&staged_file(&path)).unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains("is missing")
                && refusal.contains("To continue, put ")
                && refusal.contains(
                    &sibling(&resolve_source(&path).unwrap(), ".import-work")
                        .display()
                        .to_string()
                )
                && !refusal.contains("An exact copy"),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "work copy only").await;
    })
    .await;
}

#[tokio::test]
async fn postgres_e2_recreated_file_keeps_the_staged_copy_aside_and_warns() {
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
        let staged = staged_file(&path);
        assert_eq!(&fingerprint(&staged).unwrap(), original.get("").unwrap());
        remove_source(&path);
        let was = before(&path, &url).await;
        assert!(was.rows > 0);
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains("is missing, and PostgreSQL holds only part of its data")
                && refusal.contains(&format!("rename {} to ", staged.display())),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "missing file, unfinished import").await;
        // The documented rollback: the previous release starts, finds no
        // file and creates a new, empty database. Then the upgrade again.
        previous_release_creates_empty_database(&path).await;
        let created = inventory(&path).unwrap();
        said();
        // Round 3 reset PostgreSQL, deleted the staged copy, imported the
        // empty file and reported success: every copy was gone.
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
        let kept = kept_files(&path);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
        let warning = lines
            .iter()
            .find(|line| line.starts_with(KEPT))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(
            warning.contains(&format!(
                "an exact copy of the database as it was then is kept at {}.",
                kept[0].display()
            )) && warning.contains("you may delete it once you have checked your data"),
            "{warning}"
        );
        let smaller = lines
            .iter()
            .find(|line| line.starts_with(SMALLER))
            .unwrap_or_else(|| panic!("{lines:?}"));
        assert!(
            smaller.contains(&kept[0].display().to_string())
                && smaller.contains("users: 0 now, 3 before"),
            "{smaller}"
        );
        // The import of the present file went on and is complete.
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), created);
        assert!(usernames(&url).await.is_empty());
        assert!(!staged.exists() && !sibling(&path, ".import-work").exists());
        // Later starts leave the kept copy alone.
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
    })
    .await;
}

#[tokio::test]
async fn postgres_e3_e4_stuck_advice_offers_to_keep_postgres_first() {
    // E3: the data directory is restored from a backup taken in the middle of
    // the import, after the cutover and after normal use.
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let directory = path.parent().unwrap().to_owned();
        let backup = Directory::new();
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), backup.0.join(entry.file_name())).unwrap();
        }
        run_path(&path, &url, None).await.unwrap();
        let mut target = super::super::connect(&url).await.unwrap();
        sqlx::query("INSERT INTO instance_setting(key,value,updated_at) VALUES('after-cutover','live',TIMESTAMPTZ '2020-01-01T00:00:00Z')").execute(&mut target).await.unwrap();
        hang_up(target).await;
        for entry in fs::read_dir(&directory).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for entry in fs::read_dir(&backup.0).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), directory.join(entry.file_name())).unwrap();
        }
        let was = before(&path, &url).await;
        let source = resolve_source(&path).unwrap();
        let state = sibling(&source, ".import-work-state");
        for _ in 0..2 {
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            // Round 3 offered only "drop the database", which holds live data.
            let keep = refusal
                .find("1, to keep what PostgreSQL holds now: stop the server, move ")
                .unwrap_or_else(|| panic!("{refusal}"));
            let empty = refusal
                .find("2, only if PostgreSQL holds nothing that you need: empty the PostgreSQL database (drop it and create it again)")
                .unwrap_or_else(|| panic!("{refusal}"));
            assert!(
                refusal.starts_with("verify stage: ")
                    && keep < empty
                    && refusal.contains(&format!("delete {} and the work files ", state.display()))
                    && refusal.contains(&staged_file(&path).display().to_string())
                    && refusal.contains("this discards nothing in PostgreSQL")
                    && refusal.contains("this discards everything PostgreSQL holds now"),
                "{refusal}"
            );
            assert_data_unchanged(&path, &url, &was, "restored directory").await;
        }
        // The first way out, as written.
        let elsewhere = Directory::new();
        fs::rename(&source, elsewhere.source()).unwrap();
        cleanup(&path, &record(&path)).unwrap();
        fs::remove_file(&state).unwrap();
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(said(), Vec::<String>::new());
        assert_eq!(row_total(&url).await, was.rows);
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
    // E4: the record of one data directory is copied into another one that
    // uses the same database.
    with_database(|path, url| async move {
        unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
        let other = Directory::new();
        let b = other.source();
        golden(&b).await;
        previous_release_writes(&b).await;
        fs::copy(
            sibling(&path, ".import-work-state"),
            sibling(&b, ".import-work-state"),
        )
        .unwrap();
        run_path(&b, &url, None).await.unwrap();
        assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("verify stage: ")
                && refusal.contains("1, to keep what PostgreSQL holds now")
                && refusal.contains("2, only if PostgreSQL holds nothing that you need"),
            "{refusal}"
        );
        assert_data_unchanged(&path, &url, &was, "copied record").await;
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn postgres_e5_symbolic_link_is_never_read_as_gone() {
    // Without a record.
    with_database(|path, url| async move {
        let elsewhere = Directory::new();
        let real = elsewhere.0.join("real.sqlite3");
        fs::rename(&path, &real).unwrap();
        std::os::unix::fs::symlink(&real, &path).unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("decision stage: the SQLite file ")
                && refusal
                    .contains("it is a symbolic link; the import needs the database file itself"),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "link").await;
        // The link target is away for a moment (an unmounted volume).
        let hidden = elsewhere.0.join("hidden");
        fs::rename(&real, &hidden).unwrap();
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains("it is a symbolic link whose target cannot be read: "),
            "{refusal}"
        );
        assert_eq!(said(), Vec::<String>::new());
        assert_nothing_changed(&path, &url, &was, "dangling link").await;
    })
    .await;
    // With an unfinished record: round 3 started empty, removed the record
    // and wrote the marker; the database behind the link was never imported.
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Copy)).await.unwrap_err();
        let elsewhere = Directory::new();
        let hidden = elsewhere.0.join("hidden");
        fs::rename(&path, &hidden).unwrap();
        std::os::unix::fs::symlink(elsewhere.0.join("real.sqlite3"), &path).unwrap();
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("recovery stage: the SQLite file ")
                && refusal.contains(
                    "cannot be used: it is a symbolic link whose target cannot be read: "
                )
                && refusal.contains(
                    "An exact copy of the database as it was when the import began is at "
                ),
            "{refusal}"
        );
        assert_eq!(count_lines(&said(), "starting with an empty database"), 0);
        assert_nothing_changed(&path, &url, &was, "dangling link, record").await;
        fs::remove_file(&path).unwrap();
        fs::rename(&hidden, &path).unwrap();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
}

#[tokio::test]
async fn postgres_e6_e7_reset_never_touches_foreign_objects_or_a_table_in_use() {
    // E6a: objects that the table inventory does not list.
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let mut target = super::super::connect(&url).await.unwrap();
        sqlx::raw_sql("CREATE VIEW public.e4_view AS SELECT count(*) AS n FROM public.users; CREATE SEQUENCE public.e4_seq; SELECT nextval('public.e4_seq'); CREATE FUNCTION public.e4_fn() RETURNS int LANGUAGE sql AS 'SELECT 1'").execute(&mut target).await.unwrap();
        hang_up(target).await;
        previous_release_writes(&path).await;
        let owner = owner_bytes(&path);
        let current = inventory(&path).unwrap();
        let rows = row_total(&url).await;
        for _ in 0..2 {
            run_path(&path, &url, None).await.unwrap_err();
            // Refused: no row is gone, no work file is gone (the reset comes
            // before every cleanup), and every byte of the owner is on disk.
            assert_eq!(row_total(&url).await, rows);
            assert!(sibling(&path, ".import-work").exists());
            assert_eq!(inventory(&path).unwrap(), current);
            assert!(owner.is_subset(&hashes(&path)));
            assert!(kept_files(&path).len() <= 1);
        }
        let mut target = super::super::connect(&url).await.unwrap();
        let kept: (bool, bool, i64) = sqlx::query_as("SELECT to_regclass('public.e4_view') IS NOT NULL, to_regprocedure('public.e4_fn()') IS NOT NULL, (SELECT last_value FROM public.e4_seq)").fetch_one(&mut target).await.unwrap();
        hang_up(target).await;
        assert_eq!(kept, (true, true, 1));
    })
    .await;
    // E6b: a table of another application in another schema.
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let mut target = super::super::connect(&url).await.unwrap();
        sqlx::raw_sql("CREATE SCHEMA other; CREATE TABLE other.foreign_t(x int); INSERT INTO other.foreign_t VALUES (1)").execute(&mut target).await.unwrap();
        hang_up(target).await;
        previous_release_writes(&path).await;
        let owner = owner_bytes(&path);
        let rows = row_total(&url).await;
        run_path(&path, &url, None).await.unwrap_err();
        assert_eq!(row_total(&url).await, rows);
        assert!(sibling(&path, ".import-work").exists());
        assert!(owner.is_subset(&hashes(&path)));
        let mut target = super::super::connect(&url).await.unwrap();
        let foreign: i64 = sqlx::query_scalar("SELECT count(*) FROM other.foreign_t")
            .fetch_one(&mut target)
            .await
            .unwrap();
        hang_up(target).await;
        assert_eq!(foreign, 1);
    })
    .await;
    // E7: another session read a table inside an open transaction. The
    // reset needs ACCESS EXCLUSIVE: it waits and gives up, it never truncates
    // under the reader. And the refused reset keeps no lock. (Here PostgreSQL
    // itself aborted the transaction; the refusal by a check of this process
    // is in postgres_reset_waits_for_a_writer_and_then_refuses_its_row.)
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let rows = row_total(&url).await;
        let mut reader = super::super::connect(&url).await.unwrap();
        sqlx::raw_sql("BEGIN; SELECT count(*) FROM public.users")
            .execute(&mut reader)
            .await
            .unwrap();
        let mut target = super::super::connect(&url).await.unwrap();
        // A wait that must run out. It is the importer's own setting,
        // shortened so that the test does not take a minute.
        sqlx::raw_sql("SET lock_timeout='2s'")
            .execute(&mut target)
            .await
            .unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut target)
            .await
            .unwrap();
        reset_target(&mut target, &record(&path)).await.unwrap_err();
        let mut observer = super::super::connect(&url).await.unwrap();
        let locks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE pid=$1 AND mode='AccessExclusiveLock'",
        )
        .bind(pid)
        .fetch_one(&mut observer)
        .await
        .unwrap();
        assert_eq!(locks, 0, "a refused reset must give its locks back at once");
        hang_up(observer).await;
        sqlx::raw_sql("ROLLBACK")
            .execute(&mut reader)
            .await
            .unwrap();
        hang_up(reader).await;
        hang_up(target).await;
        assert_eq!(status(&url).await, Target::Unfinished);
        assert_eq!(row_total(&url).await, rows);
    })
    .await;
}

#[tokio::test]
async fn postgres_e8_postgres_pinned_another_work_copy_restarts_once() {
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        // A consistent directory of ANOTHER attempt (a restored backup): the
        // record pins the work copy that is on disk, PostgreSQL pinned a
        // different one.
        let work = sibling(&path, ".import-work");
        let mut conn = SqliteConnection::connect_with(&sqlite_options(&work))
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE e4_pad(x); DROP TABLE e4_pad; PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let mut journal = record(&path);
        journal.work = Some(fingerprint(&work).unwrap());
        save(&sibling(&path, ".import-work-state"), &journal).unwrap();
        said();
        // Round 3: `import stage: resume source differs from the pinned
        // source snapshot` on every start, without advice.
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
        assert!(
            lines.contains(&format!("{RESTARTING}{}", Change::Pinned.reason())),
            "{lines:?}"
        );
        // The file at the original name is unchanged, so its staged twin is
        // redundant: nothing is kept aside.
        assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
    // The same difference for an import that the command line started is
    // refused: the server never resets it.
    with_database(|path, url| async move {
        assert!(crate::pg_import::importer::run(&path, &url, Some(1))
            .await
            .is_err());
        previous_release_writes(&path).await;
        let rows = row_total(&url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with(
                "recovery stage: the unfinished command-line import cannot continue: "
            ) && refusal.contains(Change::Pinned.reason())
                && refusal.contains("drop it and create it again"),
            "{refusal}"
        );
        assert_eq!(row_total(&url).await, rows);
        assert_eq!(status(&url).await, Target::Unfinished);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn postgres_e10_unreadable_is_not_gone() {
    use std::os::unix::fs::PermissionsExt;
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        let directory = path.parent().unwrap().to_owned();
        let was = before(&path, &url).await;
        // The directory cannot be searched: the name cannot be examined.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o000)).unwrap();
        said();
        let result = run_path(&path, &url, None).await;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let refusal = result.unwrap_err().to_string();
        // The path and the OS error (mutation mD reads this as "no file").
        assert!(
            refusal.starts_with(&format!(
                "decision stage: the SQLite file {} cannot be read: ",
                path.display()
            )) && refusal.contains("(os error 13)")
                && refusal.contains("Nothing was changed"),
            "{refusal}"
        );
        assert_eq!(said(), Vec::<String>::new());
        assert_nothing_changed(&path, &url, &was, "directory mode 000").await;
        // The file cannot be read.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        said();
        let result = run_path(&path, &url, None).await;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let refusal = result.unwrap_err().to_string();
        assert!(
            refusal.starts_with("recovery stage: the SQLite file ")
                && refusal.contains("cannot be used: it cannot be opened: ")
                && refusal.contains("(os error 13)")
                && refusal.contains(&staged_file(&path).display().to_string()),
            "{refusal}"
        );
        assert_eq!(count_lines(&said(), RESTARTING), 0);
        assert_nothing_changed(&path, &url, &was, "file mode 000").await;
        // The name is a directory (Docker creates one for a missing bind
        // mount source).
        let moved = sibling(&path, ".moved-away");
        fs::rename(&path, &moved).unwrap();
        fs::create_dir(&path).unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains("cannot be used: it is a directory, not a file"),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "directory at the name").await;
        // The main file is gone and a companion is left.
        fs::remove_dir(&path).unwrap();
        fs::write(sibling(&path, "-wal"), b"left behind").unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains("cannot be used: the main file is missing while "),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "companion without main").await;
    })
    .await;
}

#[tokio::test]
async fn postgres_e11_e12_damaged_file_changes_nothing_and_names_the_staged_copy() {
    for with_rows in [false, true] {
        with_database(|path, url| async move {
            let original = inventory(&path).unwrap();
            if with_rows {
                // E12: 188 rows of our unfinished import are in PostgreSQL.
                unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
            } else {
                // E11: killed after Repair, PostgreSQL still empty.
                run_path(&path, &url, Some(Stage::Repair))
                    .await
                    .unwrap_err();
            }
            let staged = staged_file(&path);
            damage_source(&path);
            let was = before(&path, &url).await;
            assert_eq!(was.rows > 0, with_rows);
            for attempt in 0..2 {
                said();
                // Round 3: work copy and staged copy deleted (E11), rows
                // truncated as well (E12), then `invalid SQLite header` forever.
                let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
                let source = resolve_source(&path).unwrap();
                assert!(
                    refusal.starts_with("recovery stage: the SQLite file ")
                        && refusal.contains("cannot be used: it is not a SQLite database that this server can import (invalid SQLite header)")
                        && refusal.contains("Nothing was changed: no file was removed and PostgreSQL was not touched.")
                        && refusal.contains(&format!(
                            "An exact copy of the database as it was when the import began is at {}.",
                            staged.display()
                        ))
                        && refusal.contains("Do not delete that copy unless you have another one")
                        && refusal.contains(&format!(
                            "if the file is damaged: move {} out of the data directory, then rename {} to {}",
                            source.display(),
                            staged.display(),
                            source.display()
                        )),
                    "{with_rows} {attempt}: {refusal}"
                );
                let lines = said();
                assert_eq!(count_lines(&lines, RESTARTING), 0, "{lines:?}");
                assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
                assert_nothing_changed(&path, &url, &was, "damaged file").await;
                assert_eq!(&fingerprint(&staged).unwrap(), original.get("").unwrap());
                assert!(sibling(&path, ".import-work").exists());
            }
            // The way out, as written.
            fs::rename(&path, sibling(&path, ".damaged")).unwrap();
            fs::rename(&staged, &path).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert_eq!(status(&url).await, Target::Complete);
            assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
        })
        .await;
    }
}

// ---- a database of a newer release is "unusable", never restarted over ----

#[tokio::test]
async fn postgres_newer_release_file_with_a_record_changes_nothing() {
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        add_newer_migration(&path, false)
            .await
            .close()
            .await
            .unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("recovery stage: ")
                && refusal.contains("the database is from a newer release")
                && refusal.contains(&staged_file(&path).display().to_string()),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "newer release").await;
    })
    .await;
}

// ---- F5: the first line comes before any hashing ----

#[tokio::test]
async fn postgres_done_record_announces_before_it_hashes_the_work_copy() {
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Rename))
            .await
            .unwrap_err();
        let archive = archived(&path);
        assert!(archive.exists() && !path.exists());
        let state = sibling(&path, ".import-work-state");
        let mut journal = record(&path);
        journal.stage = Stage::Done;
        save(&state, &journal).unwrap();
        // Another file at the original name, and the work copy is still
        // there: it and every original component are hashed to tell this
        // from an interrupted archive rename.
        golden(&path).await;
        previous_release_writes(&path).await;
        assert!(sibling(&path, ".import-work").exists());
        // What is hashed: the work copy and the file now at the original
        // name (its archived namesake is not read: the name is taken).
        assert!(COMPONENTS[1..]
            .iter()
            .all(|suffix| !sibling(&path, suffix).exists() && !sibling(&archive, suffix).exists()));
        let scanned = fs::metadata(sibling(&path, ".import-work")).unwrap().len()
            + fs::metadata(&path).unwrap().len();
        said();
        run_path(&path, &url, None).await.unwrap();
        // The second line has the shape that a supervisor reads as work in
        // progress: "preparing" alone gives a large scan only the short wait.
        assert_eq!(
            said(),
            [
                PREPARING.to_owned(),
                format!("sqlite import: source inventory start: {scanned} bytes"),
                leave_warning(&path, &archive)
            ]
        );
    })
    .await;
}

// ---- guards of earlier rounds that no test caught (fourth checker, 13 removals) ----

const FOREIGN_ROW: &str = "INSERT INTO instance_setting(key,value,updated_at) VALUES('foreign-row','foreign-value',TIMESTAMPTZ '2020-01-01T00:00:00Z')";

#[tokio::test]
async fn postgres_reset_waits_for_a_writer_and_then_refuses_its_row() {
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let rows = row_total(&url).await;
        // Another session is in the middle of a write.
        let mut writer = super::super::connect(&url).await.unwrap();
        sqlx::raw_sql(&format!("BEGIN; {FOREIGN_ROW}"))
            .execute(&mut writer)
            .await
            .unwrap();
        let mut target = super::super::connect(&url).await.unwrap();
        let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut target)
            .await
            .unwrap();
        let mut observer = super::super::connect(&url).await.unwrap();
        let journal = record(&path);
        let reset = reset_target(&mut target, &journal);
        // The writer commits only when the reset is seen waiting for it, so
        // no timing is involved.
        let commit = async {
            loop {
                let waiting: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM pg_locks WHERE pid=$1 AND NOT granted",
                )
                .bind(pid)
                .fetch_one(&mut observer)
                .await
                .unwrap();
                if waiting > 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            sqlx::raw_sql("COMMIT").execute(&mut writer).await.unwrap();
        };
        let (result, ()) = tokio::join!(reset, commit);
        // Without its table locks the reset counts rows before the writer's
        // row exists and then truncates it away.
        let refusal = result.unwrap_err().to_string();
        assert!(refusal.contains("unowned or changed rows"), "{refusal}");
        // This refusal is a check of this process, so its transaction was
        // alive and held ACCESS EXCLUSIVE on every table. It is rolled back
        // at once: a SECOND connection looks, because any use of the first
        // one would send the rollback that a dropped transaction only queues.
        let locks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE pid=$1 AND mode='AccessExclusiveLock'",
        )
        .bind(pid)
        .fetch_one(&mut observer)
        .await
        .unwrap();
        assert_eq!(locks, 0, "a refused reset must give its locks back at once");
        hang_up(target).await;
        hang_up(observer).await;
        hang_up(writer).await;
        assert_eq!(row_total(&url).await, rows + 1);
    })
    .await;
}

#[tokio::test]
async fn postgres_reset_proves_that_it_left_an_empty_database() {
    with_database(|path, url| async move {
        unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
        let rows = row_total(&url).await;
        assert!(rows > 0);
        let mut target = super::super::connect(&url).await.unwrap();
        // Something the row and table checks of the reset do not look at, and
        // that an empty migrated database does not have.
        sqlx::raw_sql("CREATE INDEX e4_extra ON public.users (created_at)")
            .execute(&mut target)
            .await
            .unwrap();
        reset_target(&mut target, &record(&path)).await.unwrap_err();
        hang_up(target).await;
        // The refused reset removed nothing.
        assert_eq!(row_total(&url).await, rows);
    })
    .await;
}

static TEMPORARY_SEEN: std::sync::Mutex<Vec<Option<String>>> = std::sync::Mutex::new(Vec::new());

/// SQLite's process-wide temporary directory, read on a thread of its own so
/// that a plain function can ask.
fn sqlite_temporary_directory_now() -> Option<String> {
    std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let options = sqlx::sqlite::SqliteConnectOptions::new().in_memory(true);
                let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
                let now: Option<String> = sqlx::query_scalar("PRAGMA temp_store_directory")
                    .fetch_optional(&mut conn)
                    .await
                    .unwrap();
                conn.close().await.unwrap();
                now.filter(|directory| !directory.is_empty())
            })
    })
    .join()
    .unwrap()
}

fn note_temporary_directory(_: &Path, _: Stage) {
    TEMPORARY_SEEN
        .lock()
        .unwrap()
        .push(sqlite_temporary_directory_now());
}

/// The setting is one per process, so it is observed in a process of its own.
#[tokio::test]
async fn cutover_temporary_directory_fixture_child() {
    let (Some(path), Ok(url)) = (
        std::env::var_os("KOMIKA_STARTUP_CUTOVER_TEMP_PATH"),
        std::env::var("KOMIKA_STARTUP_CUTOVER_TEMP_URL"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    let fail = std::env::var_os("KOMIKA_STARTUP_CUTOVER_TEMP_FAIL").is_some();
    assert_eq!(sqlite_temporary_directory_now(), None);
    let result = run_path_with(
        &path,
        &url,
        Faults {
            stop: fail.then_some(Stage::Migrate),
            before_stage: note_temporary_directory,
            ..Faults::NONE
        },
    )
    .await;
    assert_eq!(result.is_err(), fail, "{result:?}");
    let directory = resolve_source(&path)
        .unwrap()
        .parent()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let seen = TEMPORARY_SEEN.lock().unwrap().clone();
    // Every stage runs with SQLite's temporary files in the work directory.
    assert!(
        !seen.is_empty() && seen.iter().all(|now| now.as_deref() == Some(&directory)),
        "{seen:?}"
    );
    // And the default is back afterwards, also after a failed start.
    assert_eq!(sqlite_temporary_directory_now(), None);
    std::process::exit(86);
}

#[tokio::test]
async fn postgres_cutover_points_sqlite_temporary_files_at_the_work_directory_and_back() {
    for fail in [false, true] {
        with_database(|path, url| async move {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .arg("--exact")
                .arg("pg_import::startup::tests::cutover_temporary_directory_fixture_child")
                .arg("--nocapture")
                .env("KOMIKA_STARTUP_CUTOVER_TEMP_PATH", &path)
                .env("KOMIKA_STARTUP_CUTOVER_TEMP_URL", &url);
            if fail {
                command.env("KOMIKA_STARTUP_CUTOVER_TEMP_FAIL", "1");
            }
            let output = command.output().unwrap();
            assert_eq!(
                output.status.code(),
                Some(86),
                "fail={fail}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        })
        .await;
    }
}

#[tokio::test]
async fn postgres_startup_record_does_not_own_a_command_line_import() {
    with_database(|path, url| async move {
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        // The database is recreated and the import command is started by
        // hand: an unfinished import without any startup binding.
        recreate_database(&url).await;
        assert!(crate::pg_import::importer::run(&path, &url, Some(1))
            .await
            .is_err());
        for changed in [false, true] {
            if changed {
                previous_release_writes(&path).await;
            }
            let files = listing(&path);
            let rows = row_total(&url).await;
            said();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.starts_with("recovery stage: ") && refusal.contains("did not start"),
                "{refusal}"
            );
            assert_eq!(count_lines(&said(), RESTARTING), 0);
            assert_eq!(listing(&path), files);
            assert_eq!(row_total(&url).await, rows);
            assert_eq!(status(&url).await, Target::Unfinished);
        }
    })
    .await;
}

/// A client that is not a server writes between the decision and the reset.
fn foreign_row_arrives(url: &str) {
    let url = url.to_owned();
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut other = crate::pg_import::connect(&url).await.unwrap();
                sqlx::query(FOREIGN_ROW).execute(&mut other).await.unwrap();
                let _ = other.close().await;
            })
    })
    .join()
    .unwrap();
}

#[tokio::test]
async fn postgres_refused_reset_removes_no_work_file() {
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        let current = inventory(&path).unwrap();
        let rows = row_total(&url).await;
        let owner = owner_bytes(&path);
        let work = fingerprint(&sibling(&path, ".import-work")).unwrap();
        said();
        let refusal = run_path_with(
            &path,
            &url,
            Faults {
                before_reset: foreign_row_arrives,
                ..Faults::NONE
            },
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(refusal.contains("unowned or changed rows"), "{refusal}");
        // The reset comes before any cleanup: the work copy is still there.
        assert_eq!(fingerprint(&sibling(&path, ".import-work")).unwrap(), work);
        assert_eq!(record(&path).stage, Stage::Import);
        assert_eq!(inventory(&path).unwrap(), current);
        assert_eq!(row_total(&url).await, rows + 1);
        // The staged copy was made safe first: it has its kept name.
        let kept = kept_files(&path);
        assert_eq!(kept.len(), 1);
        assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
        assert!(owner.is_subset(&hashes(&path)));
        assert_eq!(count_lines(&said(), KEPT), 1);
        // The owner removes the row. The next start resets, imports the
        // current file, and keeps no second copy.
        let mut target = super::super::connect(&url).await.unwrap();
        sqlx::query("DELETE FROM instance_setting WHERE key='foreign-row'")
            .execute(&mut target)
            .await
            .unwrap();
        hang_up(target).await;
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
        assert_eq!(count_lines(&lines, KEPT), 0, "{lines:?}");
        assert_eq!(kept_files(&path), kept);
        assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
        assert_eq!(inventory(&archived(&path)).unwrap(), current);
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn postgres_empty_start_needs_the_record_gone_first() {
    use std::os::unix::fs::PermissionsExt;
    with_database(|path, url| async move {
        fs::remove_file(&path).unwrap();
        let state = sibling(&path, ".import-work-state");
        save(
            &state,
            &Journal::fresh(uuid::Uuid::new_v4().to_string(), false),
        )
        .unwrap();
        let directory = path.parent().unwrap().to_owned();
        // The record cannot be removed.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o500)).unwrap();
        let result = run_path(&path, &url, None).await;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        // A record that outlives the marker would refuse every later start.
        result.unwrap_err();
        assert!(state.exists());
        assert!(!started_marker(&url).await);
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(said(), ["sqlite import: warning: the SQLite file named by an unfinished import is gone; starting with an empty database"]);
        assert!(!state.exists());
        assert!(started_marker(&url).await);
    })
    .await;
}

#[tokio::test]
async fn postgres_importer_lock_is_taken_before_the_state_is_read() {
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        // Another importer is in the middle of a table: it holds the lock and
        // has written rows that no checkpoint accounts for yet.
        let mut first = super::super::connect(&url).await.unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>("SELECT pg_try_advisory_lock(192837465,180002)")
                .fetch_one(&mut first)
                .await
                .unwrap()
        );
        sqlx::query(FOREIGN_ROW).execute(&mut first).await.unwrap();
        let files = listing(&path);
        // Read without the lock, that state is "unowned rows", and the start
        // would report a damaged import instead of a busy one.
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert_eq!(refusal, "decision stage: another importer owns the target");
        assert_eq!(listing(&path), files);
        sqlx::query("SELECT pg_advisory_unlock(192837465,180002)")
            .execute(&mut first)
            .await
            .unwrap();
        hang_up(first).await;
    })
    .await;
}

#[tokio::test]
async fn postgres_importer_lock_is_free_before_the_session_ends() {
    with_database(|_path, url| async move {
        const TRY: &str = "SELECT pg_try_advisory_lock(192837465,180002)";
        let mut target = super::super::connect(&url).await.unwrap();
        assert!(sqlx::query_scalar::<_, bool>(TRY)
            .fetch_one(&mut target)
            .await
            .unwrap());
        // The session stays open: PostgreSQL frees the locks of a session
        // that hung up only some time later, which no test can wait for.
        let mut open = None;
        release_with(target, |session| {
            open = Some(session);
            async { Ok(()) }
        })
        .await
        .unwrap();
        let mut next = super::super::connect(&url).await.unwrap();
        assert!(
            sqlx::query_scalar::<_, bool>(TRY)
                .fetch_one(&mut next)
                .await
                .unwrap(),
            "the next start must not find another importer"
        );
        hang_up(next).await;
        hang_up(open.unwrap()).await;
    })
    .await;
}

#[tokio::test]
async fn postgres_restarted_command_line_record_becomes_a_startup_record() {
    with_database(|path, url| async move {
        assert!(crate::pg_import::importer::run(&path, &url, Some(1))
            .await
            .is_err());
        let original = inventory(&path).unwrap();
        // The server adopts the command-line import and is killed after Copy.
        run_path(&path, &url, Some(Stage::Copy)).await.unwrap_err();
        let adopted = record(&path);
        assert!(adopted.cli_resume);
        // The database is recreated: what the record's identity was derived
        // from no longer exists.
        recreate_database(&url).await;
        said();
        run_path(&path, &url, None).await.unwrap();
        let journal = record(&path);
        assert!(!journal.cli_resume);
        assert_ne!(journal.target, adopted.target);
        assert_eq!(journal.stage, Stage::Done);
        assert_eq!(count_lines(&said(), RESTARTING), 1);
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
    })
    .await;
}

#[tokio::test]
async fn postgres_binding_is_never_replaced_while_the_database_holds_an_import() {
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        let ours = record(&path);
        let mut other = ours.clone();
        other.target = uuid::Uuid::new_v4().to_string();
        let mut target = super::super::connect(&url).await.unwrap();
        let refusal = bind_target(&mut target, &other)
            .await
            .unwrap_err()
            .to_string();
        assert!(refusal.contains("target binding changed"), "{refusal}");
        assert_eq!(
            startup_binding(&mut target).await.unwrap(),
            Some(ours.target)
        );
        hang_up(target).await;
    })
    .await;
}

// ---- the kept copy ----

#[test]
fn kept_copy_name_is_never_a_name_that_exists() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"a database").unwrap();
    let mut taken = Vec::new();
    // Three keeps within one second, and a leftover sidecar under a name.
    for suffix in ["", "-wal", "-journal"] {
        let stamp = free_kept_stamp(&path).unwrap();
        assert!(valid_stamp(&stamp) && !taken.contains(&stamp), "{stamp}");
        fs::write(sibling(&kept_path(&path, &stamp).unwrap(), suffix), b"kept").unwrap();
        taken.push(stamp);
    }
    assert!(!valid_stamp("20261011T123456"));
    assert!(kept_path(&path, "../../etc").is_err());
    let mut journal = journal_fixture(&path, Stage::Copy);
    journal.kept = Some(Kept {
        stamp: "not a stamp".into(),
        moved: false,
        complete: true,
        rows: BTreeMap::new(),
        files: BTreeMap::new(),
    });
    assert!(validate_record(&journal).is_err());
}

#[test]
fn interrupted_keep_refuses_an_occupied_name_and_loses_nothing() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"another database").unwrap();
    let state = sibling(&path, ".import-work-state");
    let mut journal = journal_fixture(&path, Stage::Import);
    let stamp = "20261011T123456Z";
    let staged = sibling(&path, ".import-work-source");
    fs::write(&staged, b"the original").unwrap();
    fs::write(sibling(&staged, "-wal"), b"its log").unwrap();
    journal.kept = Some(Kept {
        stamp: stamp.into(),
        moved: false,
        complete: true,
        rows: BTreeMap::new(),
        files: BTreeMap::from([
            (String::new(), fingerprint(&staged).unwrap()),
            (
                "-wal".to_owned(),
                fingerprint(&sibling(&staged, "-wal")).unwrap(),
            ),
        ]),
    });
    save(&state, &journal).unwrap();
    let kept = kept_path(&path, stamp).unwrap();
    // Someone else's file has the recorded name of the main file.
    fs::write(&kept, b"not ours").unwrap();
    said();
    assert!(protect_staged(&path, &state, &mut journal, None).is_err());
    assert_eq!(fs::read(&kept).unwrap(), b"not ours");
    assert_eq!(fs::read(&staged).unwrap(), b"the original");
    // The sidecar moved first; it is still on disk under the kept name.
    assert_eq!(fs::read(sibling(&kept, "-wal")).unwrap(), b"its log");
    assert_eq!(count_lines(&said(), KEPT), 0);
    assert!(!record(&path).kept.unwrap().moved);
    // Nothing may be deleted on the strength of this record.
    assert!(disposable_staged(&path, &journal).is_err());
    // The name is free again: the same start finishes the move and says so.
    fs::remove_file(&kept).unwrap();
    protect_staged(&path, &state, &mut journal, None).unwrap();
    assert_eq!(fs::read(&kept).unwrap(), b"the original");
    assert!(!staged.exists());
    assert_eq!(count_lines(&said(), KEPT), 1);
    assert!(record(&path).kept.unwrap().moved);
}

#[tokio::test]
async fn staged_copy_is_deleted_only_when_it_is_torn_or_has_a_twin() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    let state = sibling(&path, ".import-work-state");
    let staged = sibling(&path, ".import-work-source");
    let journal = journal_fixture(&path, Stage::Copy);
    save(&state, &journal).unwrap();
    // A twin of the valid file at the original name: redundant.
    fs::copy(&path, &staged).unwrap();
    assert_eq!(
        super::staged(&path, &journal).unwrap(),
        Staged::Copy { complete: true }
    );
    let mut same = journal.clone();
    protect_staged(&path, &state, &mut same, None).unwrap();
    assert!(same.kept.is_none() && staged.exists());
    disposable_staged(&path, &journal).unwrap();
    // An interrupted copy: not the recorded bytes.
    fs::write(&staged, b"half a copy").unwrap();
    assert_eq!(super::staged(&path, &journal).unwrap(), Staged::Torn);
    disposable_staged(&path, &journal).unwrap();
    let mut torn = journal.clone();
    protect_staged(&path, &state, &mut torn, None).unwrap();
    assert!(torn.kept.is_none());
    // The recorded bytes, and another valid database at the original name.
    fs::copy(&path, &staged).unwrap();
    let original = fingerprint(&staged).unwrap();
    previous_release_writes(&path).await;
    assert!(disposable_staged(&path, &journal).is_err());
    // A staged log without a twin makes the copy "not exact"; it is kept too.
    fs::write(sibling(&staged, "-wal"), b"log").unwrap();
    assert_eq!(
        super::staged(&path, &journal).unwrap(),
        Staged::Copy { complete: false }
    );
    said();
    let mut kept = journal.clone();
    protect_staged(&path, &state, &mut kept, None).unwrap();
    let name = kept_path(&path, &kept.kept.as_ref().unwrap().stamp).unwrap();
    assert_eq!(fingerprint(&name).unwrap(), original);
    assert_eq!(fs::read(sibling(&name, "-wal")).unwrap(), b"log");
    assert!(!staged.exists() && !sibling(&staged, "-wal").exists());
    let lines = said();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with(KEPT)
            && lines[0].contains("a copy of the main file (without its complete write-ahead log)")
            && lines[0].contains(&format!(
                "is kept at {} and {}.",
                name.display(),
                sibling(&name, "-wal").display()
            )),
        "{lines:?}"
    );
    // The same bytes, and a damaged file at the original name: kept as well
    // (in the cutover this state is refused before it gets here).
    fs::copy(&name, &staged).unwrap();
    fs::write(&path, vec![0u8; 4096]).unwrap();
    assert!(!staged_is_redundant(&path, &journal).unwrap());
    assert!(disposable_staged(&path, &journal).is_err());
    // Condition (ii) needs both: even a byte-identical twin is not redundant
    // while the file at the original name is not a usable database.
    fs::copy(&path, &staged).unwrap();
    assert!(!staged_is_redundant(&path, &journal).unwrap());
    fs::copy(&name, &staged).unwrap();
    // Nothing at the staged name at all.
    fs::remove_file(&staged).unwrap();
    assert_eq!(super::staged(&path, &journal).unwrap(), Staged::Absent);
    // Without a recorded original nothing can be called an interrupted copy.
    fs::write(&staged, b"unknown").unwrap();
    let mut blank = journal.clone();
    blank.original.clear();
    assert_eq!(
        super::staged(&path, &blank).unwrap(),
        Staged::Copy { complete: false }
    );
    // The cutover proof needs a Done record.
    assert!(Disposable::after_cutover(&journal).is_err());
    // Work files are removed as a set only around the staged copy.
    fs::write(sibling(&path, ".import-work"), b"derived").unwrap();
    discard_work(&path, &journal, None).unwrap();
    assert!(staged.exists() && !sibling(&path, ".import-work").exists());
}

#[test]
fn smaller_database_warning_compares_with_the_kept_copy() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"a database").unwrap();
    let mut journal = journal_fixture(&path, Stage::Copy);
    let rows = |users: u64, comments: u64| {
        BTreeMap::from([
            ("users".to_owned(), users),
            ("comments".to_owned(), comments),
        ])
    };
    journal.rows = rows(0, 7);
    assert_eq!(smaller_warning(&path, &journal), None);
    journal.kept = Some(Kept {
        stamp: "20261011T123456Z".into(),
        moved: false,
        complete: true,
        rows: rows(3, 7),
        files: BTreeMap::new(),
    });
    // Not before the copy really has its kept name.
    assert_eq!(smaller_warning(&path, &journal), None);
    journal.kept.as_mut().unwrap().moved = true;
    assert_eq!(
        smaller_warning(&path, &journal).unwrap(),
        format!(
            "{SMALLER}{} (comments: 7 now, 7 before; users: 0 now, 3 before)",
            sibling(&path, ".import-kept-20261011T123456Z").display()
        )
    );
    journal.rows = rows(3, 9);
    assert_eq!(smaller_warning(&path, &journal), None);
}

// ---- the staged copy stays byte for byte what the owner had ----

#[tokio::test]
async fn sqlite_hot_journal_fixture_child() {
    let Some(path) = std::env::var_os("KOMIKA_STARTUP_HOT_JOURNAL_TEST_PATH") else {
        return;
    };
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(PathBuf::from(path))
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .pragma("cache_size", "4")
        .foreign_keys(false)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    // Far more pages than the cache holds: changed pages reach the main file
    // and their old content the rollback journal. Then the process dies.
    sqlx::raw_sql("BEGIN; INSERT INTO users(id,username,email,password_hash,created_at) VALUES('00000000-0000-4000-8000-000000000998','hot-user','hot-user@example.invalid','fixture-secret','2020-01-01T00:00:00Z'); CREATE TABLE hot(payload BLOB); WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<400) INSERT INTO hot SELECT randomblob(4096) FROM n")
        .execute(&mut conn)
        .await
        .unwrap();
    std::process::exit(86);
}

#[tokio::test]
async fn sqlite_snapshot_leaves_the_staged_copy_byte_exact_for_wal_and_hot_journal_sources() {
    for (child, variable, sidecar) in [
        (
            "sqlite_wal_fixture_child",
            "KOMIKA_STARTUP_WAL_TEST_PATH",
            "-wal",
        ),
        (
            "sqlite_hot_journal_fixture_child",
            "KOMIKA_STARTUP_HOT_JOURNAL_TEST_PATH",
            "-journal",
        ),
    ] {
        let dir = Directory::new();
        let path = dir.source();
        golden(&path).await;
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg(format!("pg_import::startup::tests::{child}"))
            .arg("--nocapture")
            .env(variable, &path)
            .status()
            .unwrap();
        assert_eq!(status.code(), Some(86), "{child}");
        let original = inventory(&path).unwrap();
        assert!(original.contains_key(sidecar), "{child}: {original:?}");
        let journal = journal_fixture(&path, Stage::Copy);
        let rows = snapshot(&path, &original, &journal).await.unwrap();
        assert_eq!(inventory(&path).unwrap(), original, "{child}");
        // SQLite read the staged copy and changed none of its bytes: it can
        // still be told from an interrupted copy, and renamed back it IS the
        // owner's database.
        let staged = sibling(&path, ".import-work-source");
        for suffix in ["", sidecar] {
            assert_eq!(
                &fingerprint(&sibling(&staged, suffix)).unwrap(),
                original.get(suffix).unwrap(),
                "{child} {suffix}"
            );
        }
        assert_eq!(
            super::staged(&path, &journal).unwrap(),
            Staged::Copy { complete: true },
            "{child}"
        );
        // The work copy holds what a SQLite reader of the original sees: the
        // committed log applied, the unfinished transaction rolled back.
        let work = sibling(&path, ".import-work");
        assert!(!sibling(&work, "-journal").exists() && !sibling(&work, "-wal").exists());
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&work)
            .read_only(true);
        let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
        let users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        let hot: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_schema WHERE name='hot'")
            .fetch_one(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        assert_eq!(users, if sidecar == "-wal" { 4 } else { 3 }, "{child}");
        assert_eq!(hot, 0, "{child}");
        assert_eq!(rows.get("users"), Some(&(users as u64)), "{child}");
    }
}

// ---- a kill at every point of "begin again" ----

#[tokio::test]
async fn postgres_kill_at_every_point_of_a_restart_keeps_the_owners_bytes() {
    // Two sources: an unclean stop of the previous release (a write-ahead
    // log with data), and the usual one (write-ahead-log mode, closed cleanly,
    // one file).
    for (reset, clean) in [(false, false), (true, false), (false, true), (true, true)] {
        for point in [
            Crash::KeepRecorded,
            Crash::KeepHalfMoved,
            Crash::KeepMoved,
            Crash::BeforeReset,
            Crash::AfterReset,
            Crash::WorkDiscarded,
            Crash::Reserved,
        ] {
            with_database(|path, url| async move {
                let what = format!("reset={reset} clean={clean} {point:?}");
                if clean {
                    clean_wal_source(&path).await;
                } else {
                    wal_fixture(&path);
                }
                let original = inventory(&path).unwrap();
                assert_eq!(original.contains_key("-wal"), !clean);
                // The parts that hold data (a `-shm` holds none).
                let parts = original
                    .keys()
                    .filter(|suffix| DATA_COMPONENTS.contains(&suffix.as_str()))
                    .cloned()
                    .collect::<Vec<_>>();
                assert_eq!(parts.len(), if clean { 1 } else { 2 });
                if reset {
                    unfinished(&path, &url).await;
                } else {
                    run_path(&path, &url, Some(Stage::Repair))
                        .await
                        .unwrap_err();
                }
                // The previous release runs again and changes the database.
                previous_release_writes(&path).await;
                let current = inventory(&path).unwrap();
                let owner = owner_bytes(&path);
                for suffix in &parts {
                    assert!(
                        owner.contains(&original.get(suffix).unwrap().sha256),
                        "{what} {suffix:?}: {:?}",
                        listing(&path)
                    );
                }
                said();
                let outcome = run_path_with(
                    &path,
                    &url,
                    Faults {
                        crash: Some(point),
                        ..Faults::NONE
                    },
                )
                .await;
                if clean && point == Crash::KeepHalfMoved {
                    // A copy of one file is never half moved: the empty
                    // `-wal` that SQLite left beside it is no second piece,
                    // so this start passes no such point and completes.
                    outcome.unwrap();
                } else {
                    let killed = outcome.unwrap_err().to_string();
                    assert!(
                        killed.contains(&format!("test interruption at {point:?}")),
                        "{what}: {killed}"
                    );
                    assert!(owner.is_subset(&hashes(&path)), "{what}: after the kill");
                    assert_eq!(inventory(&path).unwrap(), current, "{what}");
                    run_path(&path, &url, None).await.unwrap();
                }
                assert!(owner.is_subset(&hashes(&path)), "{what}: after the import");
                // One kept copy, complete, under one name; said at least once.
                let kept = kept_files(&path);
                assert_eq!(kept.len(), 1, "{what}: {kept:?}");
                for suffix in &parts {
                    assert_eq!(
                        &fingerprint(&sibling(&kept[0], suffix)).unwrap(),
                        original.get(suffix).unwrap(),
                        "{what} {suffix}"
                    );
                }
                // Nothing else has a kept name: what SQLite left beside the
                // staged copy of a one-file source is not kept.
                assert_eq!(kept_names(&path).len(), parts.len(), "{what}");
                let lines = said();
                let warnings = lines
                    .iter()
                    .filter(|line| line.starts_with(KEPT))
                    .collect::<Vec<_>>();
                assert!(
                    !warnings.is_empty()
                        && warnings.iter().all(|line| line.contains(&if clean {
                            format!(
                                "an exact copy of the database as it was then is kept at {}. The server",
                                kept[0].display()
                            )
                        } else {
                            format!(
                                "an exact copy of the database as it was then is kept at {} and {}.",
                                kept[0].display(),
                                sibling(&kept[0], "-wal").display()
                            )
                        })),
                    "{what}: {lines:?}"
                );
                assert_eq!(status(&url).await, Target::Complete, "{what}");
                assert_eq!(inventory(&archived(&path)).unwrap(), current, "{what}");
                assert!(usernames(&url).await.contains(&"rollback-user".to_owned()));
                let left = listing(&path)
                    .into_keys()
                    .filter(|name| name.contains(".import-work") && !name.ends_with("-state"))
                    .collect::<Vec<_>>();
                assert_eq!(left, Vec::<String>::new(), "{what}");
            })
            .await;
        }
    }
}

// ---- every combination of file, database and staged copy ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FileNow {
    /// The file the import began with.
    Same,
    /// Another valid database (the previous release wrote to it).
    Different,
    Missing,
    /// 4096 zero bytes.
    Damaged,
    /// A database of a newer release.
    Newer,
    /// A directory has the name.
    Folder,
    #[cfg(unix)]
    /// Mode 000.
    Unreadable,
    #[cfg(unix)]
    /// A symbolic link to nothing.
    Dangling,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Database {
    Empty,
    /// Our own unfinished import.
    Unfinished,
    /// Our own import, complete and not yet verified.
    Complete,
    /// Data that no import of this directory put there.
    Foreign,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expected {
    Refused,
    Imported { kept: bool },
}

fn expected(file: FileNow, database: Database, staged: bool) -> Expected {
    match (database, file) {
        (Database::Foreign, _) => Expected::Refused,
        (_, FileNow::Same) => Expected::Imported { kept: false },
        (Database::Complete, FileNow::Different) => Expected::Refused,
        (_, FileNow::Different) => Expected::Imported { kept: staged },
        _ => Expected::Refused,
    }
}

/// The property of fix round 4, for one database state and every file state,
/// with and without a staged copy: after the start every byte of the owner's
/// database that was on disk before it is still on disk; a refused start
/// changed nothing at all; an accepted start imported the file that was at
/// the original name, completely and verified.
async fn owner_never_loses_the_last_copy(database: Database, clean_wal: bool) {
    let files = [
        FileNow::Same,
        FileNow::Different,
        FileNow::Missing,
        FileNow::Damaged,
        FileNow::Newer,
        FileNow::Folder,
        #[cfg(unix)]
        FileNow::Unreadable,
        #[cfg(unix)]
        FileNow::Dangling,
    ];
    for file in files {
        for staged in [true, false] {
            with_database(|path, url| async move {
                let what = format!("{database:?} {file:?} staged={staged} clean_wal={clean_wal}");
                if clean_wal {
                    clean_wal_source(&path).await;
                }
                let original = inventory(&path).unwrap();
                match database {
                    Database::Empty | Database::Foreign => {
                        run_path(&path, &url, Some(Stage::Repair))
                            .await
                            .unwrap_err();
                    }
                    Database::Unfinished => {
                        unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1)
                            .await
                    }
                    Database::Complete => {
                        run_path(&path, &url, Some(Stage::Import))
                            .await
                            .unwrap_err();
                    }
                }
                if database == Database::Foreign {
                    recreate_database(&url).await;
                    let mut target = super::super::connect(&url).await.unwrap();
                    sqlx::raw_sql("CREATE TABLE public.somebody_else(x int); INSERT INTO public.somebody_else VALUES (1)")
                        .execute(&mut target)
                        .await
                        .unwrap();
                    hang_up(target).await;
                }
                let copy = staged_file(&path);
                assert_eq!(&fingerprint(&copy).unwrap(), original.get("").unwrap());
                if !staged {
                    for suffix in COMPONENTS {
                        remove_owned(&sibling(&copy, suffix)).unwrap();
                    }
                }
                let elsewhere = Directory::new();
                match file {
                    FileNow::Same => {}
                    FileNow::Different => previous_release_writes(&path).await,
                    FileNow::Missing => remove_source(&path),
                    FileNow::Damaged => damage_source(&path),
                    FileNow::Newer => add_newer_migration(&path, false)
                        .await
                        .close()
                        .await
                        .unwrap(),
                    FileNow::Folder => {
                        fs::rename(&path, elsewhere.source()).unwrap();
                        fs::create_dir(&path).unwrap();
                    }
                    #[cfg(unix)]
                    FileNow::Unreadable => {}
                    #[cfg(unix)]
                    FileNow::Dangling => {
                        fs::rename(&path, elsewhere.source()).unwrap();
                        std::os::unix::fs::symlink(elsewhere.0.join("nothing"), &path).unwrap();
                    }
                }
                let owner = owner_bytes(&path);
                let present = inventory(&path).ok();
                let was = before(&path, &url).await;
                #[cfg(unix)]
                if file == FileNow::Unreadable {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
                }
                said();
                let result = run_path(&path, &url, None).await;
                #[cfg(unix)]
                if file == FileNow::Unreadable {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                }
                let lines = said();
                // The property.
                let after = hashes(&path);
                assert!(
                    owner.is_subset(&after),
                    "{what}: bytes of the owner's database are gone: {result:?} {lines:?}"
                );
                match expected(file, database, staged) {
                    Expected::Refused => {
                        let refusal = result.unwrap_err().to_string();
                        assert!(
                            refusal.starts_with("recovery stage: ")
                                && refusal.contains("Nothing was changed"),
                            "{what}: {refusal}"
                        );
                        assert_eq!(count_lines(&lines, RESTARTING), 0, "{what}: {lines:?}");
                        assert_eq!(count_lines(&lines, "starting with an empty"), 0, "{what}");
                        assert_nothing_changed(&path, &url, &was, &what).await;
                        // The refusal names the staged copy whenever the file
                        // at the original name is not a usable database.
                        let unusable = !matches!(file, FileNow::Same | FileNow::Different);
                        if staged && unusable && database != Database::Foreign {
                            assert!(
                                refusal.contains(&format!(
                                    "An exact copy of the database as it was when the import began is at {}.",
                                    copy.display()
                                )),
                                "{what}: {refusal}"
                            );
                        }
                    }
                    Expected::Imported { kept } => {
                        result.unwrap_or_else(|error| panic!("{what}: {error}"));
                        assert_eq!(record(&path).stage, Stage::Done, "{what}");
                        assert_eq!(status(&url).await, Target::Complete, "{what}");
                        assert_eq!(
                            Some(inventory(&archived(&path)).unwrap()),
                            present,
                            "{what}: the archive is the file that was imported"
                        );
                        assert_eq!(
                            usernames(&url).await.contains(&"rollback-user".to_owned()),
                            file == FileNow::Different,
                            "{what}"
                        );
                        let copies = kept_files(&path);
                        assert_eq!(copies.len(), usize::from(kept), "{what}: {copies:?}");
                        assert_eq!(count_lines(&lines, KEPT), usize::from(kept), "{what}");
                        if kept {
                            assert_eq!(
                                &fingerprint(&copies[0]).unwrap(),
                                original.get("").unwrap(),
                                "{what}"
                            );
                        }
                        assert!(!copy.exists() && !sibling(&path, ".import-work").exists());
                    }
                }
            })
            .await;
        }
    }
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_empty_database() {
    owner_never_loses_the_last_copy(Database::Empty, false).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_empty_database_clean_wal_source() {
    owner_never_loses_the_last_copy(Database::Empty, true).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_unfinished_import() {
    owner_never_loses_the_last_copy(Database::Unfinished, false).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_unfinished_import_clean_wal_source() {
    owner_never_loses_the_last_copy(Database::Unfinished, true).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_complete_import() {
    owner_never_loses_the_last_copy(Database::Complete, false).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_complete_import_clean_wal_source() {
    owner_never_loses_the_last_copy(Database::Complete, true).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_foreign_data() {
    owner_never_loses_the_last_copy(Database::Foreign, false).await;
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_foreign_data_clean_wal_source() {
    owner_never_loses_the_last_copy(Database::Foreign, true).await;
}

// ===================== Fix round 5 =====================
//
// The fifth checker's findings R1 to R6, the five blind spots it listed, and
// the round-4 guards whose removal no test of round 4 noticed.

/// An unclean stop of the previous release: one committed row is only in the
/// write-ahead log.
fn wal_fixture(path: &Path) {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("pg_import::startup::tests::sqlite_wal_fixture_child")
        .arg("--nocapture")
        .env("KOMIKA_STARTUP_WAL_TEST_PATH", path)
        .status()
        .unwrap();
    assert_eq!(child.code(), Some(86));
}

/// Do what a refusal says, to the letter: after `after`, first the files
/// that it says to move out of the data directory (they go to the directory
/// that is returned), then the renames, up to " and start again". A rename
/// onto a name that is taken fails the test: it would replace that file.
fn follow_renames(refusal: &str, after: &str) -> Directory {
    let start = refusal
        .find(after)
        .unwrap_or_else(|| panic!("{after:?} is not in: {refusal}"))
        + after.len();
    let rest = &refusal[start..];
    let steps = &rest[..rest.find(" and start again").expect("the end of the step")];
    // The steps may stand in brackets.
    let steps = steps.strip_suffix(')').unwrap_or(steps);
    // The name of a kept copy and what it holds may come first.
    let first = [steps.find("move "), steps.find("rename ")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or_else(|| panic!("{steps}"));
    let steps = &steps[first..];
    let out = Directory::new();
    let steps = match steps.strip_prefix("move ") {
        Some(rest) => {
            let (files, then) = rest
                .split_once(" out of the data directory, then ")
                .unwrap_or_else(|| panic!("{steps}"));
            for file in files.split(", ") {
                let name = Path::new(file).file_name().unwrap();
                fs::rename(file, out.0.join(name))
                    .unwrap_or_else(|error| panic!("{file}: {error}"));
            }
            then
        }
        None => steps,
    };
    let steps = steps
        .strip_prefix("rename ")
        .unwrap_or_else(|| panic!("{steps}"));
    // "..., delete A, B" may end the step: work files, by their names.
    let (steps, delete) = match steps.split_once(", delete ") {
        Some((steps, delete)) => (steps, Some(delete)),
        None => (steps, None),
    };
    for step in steps.split(", ") {
        let (from, to) = step.split_once(" to ").unwrap_or_else(|| panic!("{step}"));
        assert!(fs::symlink_metadata(to).is_err(), "{to} is taken");
        fs::rename(from, to).unwrap();
    }
    for file in delete.into_iter().flat_map(|files| files.split(", ")) {
        assert!(file.contains(".import-work"), "{file} is no work file");
        fs::remove_file(file).unwrap();
    }
    out
}

async fn killed_at(path: &Path, url: &str, point: Crash) {
    let killed = run_path_with(
        path,
        url,
        Faults {
            crash: Some(point),
            ..Faults::NONE
        },
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        killed.contains(&format!("test interruption at {point:?}")),
        "{killed}"
    );
}

// ---- R1: a kept move that stopped half-way ----

#[tokio::test]
async fn postgres_half_moved_kept_copy_is_put_back_whole_by_the_printed_instructions() {
    // The owner follows the refusal after the original is lost.
    with_database(|path, url| async move {
        wal_fixture(&path);
        let original = inventory(&path).unwrap();
        assert!(original.contains_key("-wal"));
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        // Killed after the staged log has its kept name and before the main
        // file moved.
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        let kept = record(&path).kept.unwrap();
        assert!(!kept.moved);
        let aside = kept_path(&source, &kept.stamp).unwrap();
        let log = sibling(&aside, "-wal");
        assert!(staged.exists() && !sibling(&staged, "-wal").exists());
        assert!(log.exists() && !aside.exists());
        // The original is lost.
        remove_source(&path);
        let was = before(&path, &url).await;
        for _ in 0..2 {
            said();
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            // Round 4 named the staged main file only: the owner got a
            // database without the row that was committed in the log.
            assert!(
                refusal.contains("is missing, and PostgreSQL holds only part of its data")
                    && refusal.contains("Nothing was changed")
                    && refusal.contains(&format!(
                        "An exact copy of the database as it was when the import began was being moved aside when an earlier start stopped, and is now at these names: the main file at {}; the write-ahead log at {}.",
                        staged.display(),
                        log.display()
                    ))
                    && refusal.contains(&format!(
                        "To continue, rename {} to {}, {} to {} and start again.",
                        staged.display(),
                        source.display(),
                        log.display(),
                        sibling(&source, "-wal").display()
                    )),
                "{refusal}"
            );
            assert_eq!(count_lines(&said(), RESTARTING), 0);
            assert_nothing_changed(&path, &url, &was, "half-moved copy, file lost").await;
        }
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        follow_renames(&refusal, "To continue, ");
        for suffix in ["", "-wal"] {
            assert_eq!(
                &fingerprint(&sibling(&path, suffix)).unwrap(),
                original.get(suffix).unwrap()
            );
        }
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        // The row that was only in the log is in PostgreSQL.
        let names = usernames(&url).await;
        assert!(names.contains(&"wal-owner".to_owned()), "{names:?}");
        assert!(!names.contains(&"rollback-user".to_owned()), "{names:?}");
        let archive = archived(&path);
        for suffix in ["", "-wal"] {
            assert_eq!(
                &fingerprint(&sibling(&archive, suffix)).unwrap(),
                original.get(suffix).unwrap()
            );
        }
        // Nothing has a kept name, so nothing was announced as kept.
        assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
        assert!(record(&path).kept.is_none());
        assert_eq!(count_lines(&said(), KEPT), 0);
    })
    .await;
    // The owner puts back the main file only (what round 4 told them to do).
    with_database(|path, url| async move {
        wal_fixture(&path);
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        let kept = record(&path).kept.unwrap();
        let log = sibling(&kept_path(&source, &kept.stamp).unwrap(), "-wal");
        remove_source(&path);
        fs::rename(&staged, &path).unwrap();
        let was = before(&path, &url).await;
        said();
        // Round 4 marked the move finished, announced "an exact copy" at a
        // name where no main file was, and imported the file without the
        // committed row.
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.starts_with("recovery stage: an earlier start was interrupted while it moved the copy")
                && refusal.contains(&format!(
                    "that copy is not complete now: the main file was put back at {}; the write-ahead log is at {}.",
                    source.display(),
                    log.display()
                ))
                && refusal.contains("Nothing was changed")
                && refusal.contains(&format!(
                    "To finish putting that database back, rename {} to {} and start again.",
                    log.display(),
                    sibling(&source, "-wal").display()
                ))
                && refusal.contains(&format!(
                    "To go on without that copy, move {} out of the data directory",
                    log.display()
                )),
            "{refusal}"
        );
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 0, "{lines:?}");
        assert_eq!(count_lines(&lines, KEPT), 0, "{lines:?}");
        assert_nothing_changed(&path, &url, &was, "kept set in two places").await;
        assert!(!record(&path).kept.unwrap().moved);
        follow_renames(&refusal, "To finish putting that database back, ");
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        assert!(usernames(&url).await.contains(&"wal-owner".to_owned()));
        assert_eq!(
            &fingerprint(&sibling(&archived(&path), "-wal")).unwrap(),
            original.get("-wal").unwrap()
        );
        assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
    })
    .await;
    // The original is lost and so is the staged main file: what is left is a
    // part of the copy, and no refusal offers a part as the way back.
    with_database(|path, url| async move {
        wal_fixture(&path);
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let kept = record(&path).kept.unwrap();
        let log = sibling(&kept_path(&source, &kept.stamp).unwrap(), "-wal");
        remove_source(&path);
        let elsewhere = Directory::new();
        fs::rename(staged_file(&path), elsewhere.source()).unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "A part of the copy of the database as it was when the import began was being moved aside when an earlier start stopped, and is now at these names: the main file missing; the write-ahead log at {}.",
                log.display()
            )) && refusal.contains(&format!(
                "To continue, put {} back and start again.",
                source.display()
            )) && !refusal.contains(&format!("rename {}", log.display())),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "a part of the copy").await;
    })
    .await;
}

#[test]
fn interrupted_keep_is_never_called_moved_without_every_recorded_piece() {
    let dir = Directory::new();
    let path = dir.source();
    fs::write(&path, b"another database").unwrap();
    let state = sibling(&path, ".import-work-state");
    let staged = sibling(&path, ".import-work-source");
    let stamp = "20261011T123456Z";
    let kept = kept_path(&path, stamp).unwrap();
    let fixture = |main: Option<&[u8]>, log_moved: bool| {
        for file in [
            &staged,
            &sibling(&staged, "-wal"),
            &kept,
            &sibling(&kept, "-wal"),
        ] {
            let _ = fs::remove_file(file);
        }
        fs::write(&staged, b"the original").unwrap();
        fs::write(sibling(&staged, "-wal"), b"its log").unwrap();
        let mut journal = journal_fixture(&path, Stage::Import);
        journal.kept = Some(Kept {
            stamp: stamp.into(),
            moved: false,
            complete: true,
            rows: BTreeMap::new(),
            files: BTreeMap::from([
                (String::new(), fingerprint(&staged).unwrap()),
                (
                    "-wal".to_owned(),
                    fingerprint(&sibling(&staged, "-wal")).unwrap(),
                ),
            ]),
        });
        let _ = fs::remove_file(&state);
        save(&state, &journal).unwrap();
        if log_moved {
            fs::rename(sibling(&staged, "-wal"), sibling(&kept, "-wal")).unwrap();
        }
        fs::remove_file(&staged).unwrap();
        if let Some(bytes) = main {
            fs::write(&kept, bytes).unwrap();
        }
        journal
    };
    // The log has its kept name and the main file is nowhere.
    let mut journal = fixture(None, true);
    said();
    let refusal = protect_staged(&path, &state, &mut journal, None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        refusal.contains(&format!(
            "that copy is not complete now: the main file is missing; the write-ahead log is at {}.",
            sibling(&kept, "-wal").display()
        )) && !refusal.contains("To finish putting")
            && refusal.contains("To go on without that copy, move "),
        "{refusal}"
    );
    // The file at the original name is hashed to see whether it is the
    // missing piece, and the scan is announced first. Nothing is called kept.
    assert_eq!(said(), ["sqlite import: source inventory start: 16 bytes"]);
    assert!(!record(&path).kept.unwrap().moved);
    assert!(disposable_staged(&path, &journal).is_err());
    assert_eq!(fs::read(sibling(&kept, "-wal")).unwrap(), b"its log");
    // The owner takes the last piece away, as the refusal says: nothing is
    // left to keep, and nothing is announced as kept.
    fs::remove_file(sibling(&kept, "-wal")).unwrap();
    protect_staged(&path, &state, &mut journal, None).unwrap();
    assert!(journal.kept.is_none() && record(&path).kept.is_none());
    assert_eq!(count_lines(&said(), KEPT), 0);
    // Every piece has its kept name, but the main file there is another file.
    let mut journal = fixture(Some(b"not the original"), true);
    let refusal = protect_staged(&path, &state, &mut journal, None)
        .err()
        .unwrap()
        .to_string();
    assert!(
        refusal.contains("does not hold the bytes of the copy that was being kept"),
        "{refusal}"
    );
    assert_eq!(count_lines(&said(), KEPT), 0);
    assert!(!record(&path).kept.unwrap().moved);
    assert_eq!(fs::read(&kept).unwrap(), b"not the original");
    // The same with the right bytes: now, and only now, it is called moved.
    let mut journal = fixture(Some(b"the original"), true);
    protect_staged(&path, &state, &mut journal, None).unwrap();
    assert!(record(&path).kept.unwrap().moved);
    assert_eq!(count_lines(&said(), KEPT), 1);
}

// ---- R2: no empty start beside a kept copy ----

#[tokio::test]
async fn postgres_no_empty_start_beside_a_kept_copy() {
    for point in [Crash::WorkDiscarded, Crash::Reserved] {
        for with_record in [true, false] {
            with_database(|path, url| async move {
                let what = format!("{point:?} record={with_record}");
                let original = inventory(&path).unwrap();
                unfinished(&path, &url).await;
                previous_release_writes(&path).await;
                // PostgreSQL is reset and the work files are gone: the
                // database of the earlier attempt exists as a kept copy only.
                killed_at(&path, &url, point).await;
                assert_eq!(status(&url).await, Target::Empty, "{what}");
                let kept = kept_files(&path);
                assert_eq!(kept.len(), 1, "{what}");
                assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
                let source = resolve_source(&path).unwrap();
                let state = sibling(&source, ".import-work-state");
                assert_eq!(
                    listing(&path)
                        .into_keys()
                        .filter(|name| name.contains(".import-work") && !name.ends_with("-state"))
                        .collect::<Vec<_>>(),
                    Vec::<String>::new(),
                    "{what}"
                );
                // The file is moved out of the directory before the next start.
                let elsewhere = Directory::new();
                for suffix in COMPONENTS {
                    let _ =
                        fs::rename(sibling(&path, suffix), sibling(&elsewhere.source(), suffix));
                }
                if !with_record {
                    fs::remove_file(&state).unwrap();
                }
                let was = before(&path, &url).await;
                for _ in 0..2 {
                    said();
                    // Round 4 started with an empty database here and wrote
                    // the marker that turns the import off for good.
                    let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
                    assert!(
                        refusal.contains(&format!("the SQLite file {} ", source.display()))
                            && refusal.contains("is missing")
                            && refusal.contains("Nothing was changed")
                            && refusal.contains(&kept[0].display().to_string())
                            && refusal.contains(&format!(
                                "rename {} to {} and start again",
                                kept[0].display(),
                                source.display()
                            ))
                            && refusal.contains(&format!(
                                "move {} out of the data directory",
                                if with_record {
                                    kept[0].display().to_string()
                                } else {
                                    "the listed files".to_owned()
                                }
                            )),
                        "{what}: {refusal}"
                    );
                    assert_eq!(
                        count_lines(&said(), "starting with an empty database"),
                        0,
                        "{what}"
                    );
                    // Also: no normal-start marker.
                    assert_nothing_changed(&path, &url, &was, &what).await;
                }
                if point == Crash::WorkDiscarded {
                    // Way out: import the kept copy, as written.
                    let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
                    follow_renames(
                        &refusal,
                        if with_record {
                            "Or, to import the kept copy "
                        } else {
                            "To import the newest one, "
                        },
                    );
                    run_path(&path, &url, None).await.unwrap();
                    assert_eq!(status(&url).await, Target::Complete, "{what}");
                    assert_eq!(
                        inventory(&archived(&path)).unwrap().get(""),
                        original.get(""),
                        "{what}"
                    );
                } else {
                    // Way out: the empty database, as written.
                    if with_record {
                        fs::remove_file(&state).unwrap();
                    }
                    fs::rename(&kept[0], elsewhere.0.join("kept")).unwrap();
                    said();
                    run_path(&path, &url, None).await.unwrap();
                    assert_eq!(said(), Vec::<String>::new(), "{what}");
                    assert!(started_marker(&url).await, "{what}");
                }
            })
            .await;
        }
    }
    // The exception: after a COMPLETED import the kept copy stays for good,
    // and no later start is refused or warned because of it, with the Done
    // record and without it.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        let kept = kept_files(&path);
        assert_eq!(kept.len(), 1);
        for with_record in [true, false] {
            if !with_record {
                fs::remove_file(sibling(&path, ".import-work-state")).unwrap();
            }
            said();
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(said(), Vec::<String>::new(), "record={with_record}");
            assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
        }
    })
    .await;
}

// ---- R3: a target that an older release migrated ----

fn applied_all() -> Vec<i64> {
    let mut versions = crate::pg_import::importer::POSTGRES
        .iter()
        .map(|m| m.version)
        .collect::<Vec<_>>();
    versions.sort();
    versions
}

/// The PostgreSQL migrations of the release before this one: all but the last.
fn older_migrator() -> sqlx::migrate::Migrator {
    let latest = *applied_all().last().unwrap();
    sqlx::migrate::Migrator {
        migrations: Cow::Owned(
            crate::pg_import::importer::POSTGRES
                .iter()
                .filter(|m| m.version < latest)
                .cloned()
                .collect(),
        ),
        ..sqlx::migrate::Migrator::DEFAULT
    }
}

async fn applied(url: &str) -> Vec<i64> {
    let mut target = super::super::connect(url).await.unwrap();
    let versions = sqlx::query_scalar("SELECT version FROM public._sqlx_migrations ORDER BY 1")
        .fetch_all(&mut target)
        .await
        .unwrap();
    hang_up(target).await;
    versions
}

/// The importer of that older release, on a real database: its migrator
/// creates the schema, and the importer pins what it created.
async fn older_import(source: &Path, url: &str, stop_after: Option<usize>) -> Result<()> {
    let mut target = super::super::connect(url).await?;
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(192837465,180002)")
        .fetch_one(&mut target)
        .await?;
    assert!(locked);
    let result = crate::pg_import::importer::run_with_migrator(
        source,
        &mut target,
        stop_after,
        &older_migrator(),
    )
    .await;
    release(target).await?;
    result.map(|_| ())
}

/// A startup import of the older release up to the Import stage: record,
/// binding and pinned work copy, with PostgreSQL still untouched by a migrator.
async fn older_release_prepared(path: &Path, url: &str) -> PathBuf {
    run_path(path, url, Some(Stage::Repair)).await.unwrap_err();
    let work = sibling(path, ".import-work");
    let mut journal = record(path);
    journal.stage = Stage::Import;
    journal.work = Some(fingerprint(&work).unwrap());
    // That release had other migrations, so it recorded another value.
    journal.prepared_migrations = "the migrations of an older release".into();
    save(&sibling(path, ".import-work-state"), &journal).unwrap();
    work
}

#[tokio::test]
async fn postgres_unfinished_import_of_an_older_migration_level_is_restarted_and_completes() {
    let older = applied_all();
    let older = &older[..older.len() - 1];
    assert!(!older.is_empty());
    // A startup import, killed after its first table.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        let work = older_release_prepared(&path, &url).await;
        older_import(&work, &url, Some(1)).await.unwrap_err();
        assert_eq!(applied(&url).await, older);
        // The state is recognised at the level it has.
        assert_eq!(status(&url).await, Target::Unfinished);
        said();
        // Round 4: the reset proved "empty" against the indexes of the latest
        // migration, failed, and rolled back, on every start.
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
        assert!(
            lines.contains(&format!("{RESTARTING}{}", Change::Binary.reason())),
            "{lines:?}"
        );
        assert_eq!(applied(&url).await, applied_all());
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
    })
    .await;
    // An import that the command line of the older release began. The server
    // never resets it: it is resumed, and the importer's migrator brings the
    // schema up to date under the same pinned-schema proof.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        older_import(
            &path,
            &url,
            Some(crate::pg_import::plan::tables().len() - 1),
        )
        .await
        .unwrap_err();
        assert_eq!(applied(&url).await, older);
        assert_eq!(status(&url).await, Target::Unfinished);
        let rows = row_total(&url).await;
        assert!(rows > 0);
        said();
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 0, "{lines:?}");
        assert_eq!(applied(&url).await, applied_all());
        assert_eq!(status(&url).await, Target::Complete);
        assert!(row_total(&url).await > rows);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
    })
    .await;
}

#[tokio::test]
async fn postgres_completed_import_of_an_older_migration_level_is_verified_and_archived() {
    // Done: the record was published and the archive rename was not finished.
    for stage in [Stage::Verify, Stage::Rename, Stage::Done] {
        with_database(|path, url| async move {
            let older = applied_all();
            let older = &older[..older.len() - 1];
            let original = inventory(&path).unwrap();
            let work = older_release_prepared(&path, &url).await;
            older_import(&work, &url, None).await.unwrap();
            assert_eq!(applied(&url).await, older);
            assert_eq!(status(&url).await, Target::Complete);
            let rows = row_total(&url).await;
            // Killed before the archive rename.
            let mut journal = record(&path);
            journal.stage = stage;
            if stage != Stage::Verify {
                journal.archive_stamp = Some("20261011T123456Z".into());
            }
            save(&sibling(&path, ".import-work-state"), &journal).unwrap();
            said();
            // Round 4: the verifier compared the indexes with the latest
            // migration before anything could apply it, on every start. (For
            // the Done record it warned and left the file where it was.)
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            assert_eq!(count_lines(&lines, RESTARTING), 0, "{stage:?}: {lines:?}");
            assert_eq!(
                count_lines(&lines, "sqlite import: warning: "),
                0,
                "{stage:?}: {lines:?}"
            );
            // The Rename stage verifies again without the line.
            assert_eq!(
                count_lines(&lines, "sqlite import: verifier PASS: "),
                usize::from(stage != Stage::Rename),
                "{stage:?}: {lines:?}"
            );
            assert_eq!(applied(&url).await, applied_all(), "{stage:?}");
            assert_eq!(status(&url).await, Target::Complete, "{stage:?}");
            // The same rows: a completed import is never imported again.
            assert_eq!(row_total(&url).await, rows, "{stage:?}");
            assert_eq!(inventory(&archived(&path)).unwrap(), original, "{stage:?}");
            assert_eq!(record(&path).stage, Stage::Done);
        })
        .await;
    }
}

#[tokio::test]
async fn postgres_migrated_empty_target_of_an_older_migration_level_is_adopted() {
    with_database(|path, url| async move {
        let older = applied_all();
        let older = &older[..older.len() - 1];
        let original = inventory(&path).unwrap();
        let mut target = super::super::connect(&url).await.unwrap();
        older_migrator().run_direct(&mut target).await.unwrap();
        hang_up(target).await;
        assert_eq!(applied(&url).await, older);
        // Round 4 could not even tell that this database is empty.
        assert_eq!(status(&url).await, Target::Empty);
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(applied(&url).await, applied_all());
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
    })
    .await;
    // What stays refused: an applied migration that this server does not
    // have, or has with other content, and an index that no applied migration
    // creates. Nothing is changed.
    for (change, message) in [
        (
            "UPDATE public._sqlx_migrations SET checksum='\\x00' WHERE version=(SELECT min(version) FROM public._sqlx_migrations)",
            "differs from the migration this server embeds",
        ),
        (
            "UPDATE public._sqlx_migrations SET version=version+1000 WHERE version=(SELECT max(version) FROM public._sqlx_migrations)",
            "is not the next migration of this server",
        ),
        (
            "UPDATE public._sqlx_migrations SET success=false WHERE version=(SELECT max(version) FROM public._sqlx_migrations)",
            "did not complete",
        ),
        (
            "CREATE INDEX not_of_a_migration ON public.users (username)",
            "PostgreSQL indexes differ from the migrations_pg files",
        ),
        (
            "INSERT INTO public._sqlx_migrations(version,description,success,checksum,execution_time) SELECT max(version)+m,'from a newer release',true,'\\x00',0 FROM public._sqlx_migrations, generate_series(1,2) m GROUP BY m",
            "it belongs to a newer release",
        ),
    ] {
        with_database(|path, url| async move {
            let mut target = super::super::connect(&url).await.unwrap();
            older_migrator().run_direct(&mut target).await.unwrap();
            sqlx::raw_sql(change).execute(&mut target).await.unwrap();
            let refusal = inspect(&mut target).await.unwrap_err().to_string();
            assert!(refusal.contains(message), "{refusal}");
            hang_up(target).await;
            let files = listing(&path);
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(refusal.contains(message), "{refusal}");
            assert_eq!(listing(&path), files);
            assert!(!started_marker(&url).await);
        })
        .await;
    }
}

// ---- R4: a Verify record over a database that was put back ----

#[test]
fn resume_stage_follows_what_postgres_holds() {
    for stage in [
        Stage::Copy,
        Stage::Migrate,
        Stage::Repair,
        Stage::Import,
        Stage::Verify,
        Stage::Rename,
        Stage::Done,
    ] {
        for target in [
            Target::Empty,
            Target::Unfinished,
            Target::Complete,
            Target::Other,
        ] {
            let expected = if (stage, target) == (Stage::Verify, Target::Unfinished) {
                Stage::Import
            } else {
                stage
            };
            assert_eq!(
                resume_stage(stage, target),
                expected,
                "{stage:?} {target:?}"
            );
        }
    }
    // A Rename record without its completed import stays refused.
    assert_eq!(
        pending(
            Stage::Rename,
            Target::Unfinished,
            true,
            false,
            Source::Valid,
            None,
            true
        ),
        Pending::Refuse(Refusal::RenameWithoutImport)
    );
}

#[tokio::test]
async fn postgres_verify_record_over_a_restored_unfinished_target_resumes_the_import() {
    for other_copy in [false, true] {
        with_database(|path, url| async move {
            let original = inventory(&path).unwrap();
            run_path(&path, &url, Some(Stage::Verify))
                .await
                .unwrap_err();
            let journal = record(&path);
            assert_eq!(journal.stage, Stage::Verify);
            assert_eq!(status(&url).await, Target::Complete);
            // PostgreSQL is put back to a point in the middle of this import:
            // the record's binding, and one table.
            recreate_database(&url).await;
            let mut target = super::super::connect(&url).await.unwrap();
            bind_target(&mut target, &journal).await.unwrap();
            hang_up(target).await;
            let work = sibling(&path, ".import-work");
            let elsewhere = Directory::new();
            let pinned = if other_copy {
                // ... of an attempt that had prepared another work copy.
                let other = elsewhere.source();
                fs::copy(&work, &other).unwrap();
                let mut conn = SqliteConnection::connect_with(&sqlite_options(&other))
                    .await
                    .unwrap();
                sqlx::raw_sql(
                    "CREATE TABLE e5_pad(x); DROP TABLE e5_pad; PRAGMA wal_checkpoint(TRUNCATE)",
                )
                .execute(&mut conn)
                .await
                .unwrap();
                conn.close().await.unwrap();
                other
            } else {
                work.clone()
            };
            assert!(crate::pg_import::importer::run(&pinned, &url, Some(1))
                .await
                .is_err());
            assert_eq!(status(&url).await, Target::Unfinished);
            said();
            // Round 4 kept the Verify stage: the verifier failed on every
            // start, with advice that could not be followed.
            run_path(&path, &url, None)
                .await
                .unwrap_or_else(|error| panic!("other_copy={other_copy}: {error}"));
            let lines = said();
            assert_eq!(
                count_lines(&lines, RESTARTING),
                usize::from(other_copy),
                "{lines:?}"
            );
            if other_copy {
                assert!(
                    lines.contains(&format!("{RESTARTING}{}", Change::Pinned.reason())),
                    "{lines:?}"
                );
            }
            assert_eq!(
                count_lines(&lines, "sqlite import: import start"),
                1 + usize::from(other_copy),
                "{lines:?}"
            );
            assert_eq!(count_lines(&lines, "sqlite import: verifier PASS: "), 1);
            assert_eq!(status(&url).await, Target::Complete);
            assert_eq!(inventory(&archived(&path)).unwrap(), original);
            assert_eq!(kept_files(&path), Vec::<PathBuf>::new());
        })
        .await;
    }
}

thread_local! {
    /// The database of the test on this thread, and how often its start has
    /// reached the Import stage.
    static IMPORT_VISITS: RefCell<(String, u32)> = const { RefCell::new((String::new(), 0)) };
}

/// Between the restart of a start and its second Import stage, a third party
/// makes PostgreSQL pin yet another copy: the importer's own state tables,
/// with a source identity that no work copy has.
fn another_copy_is_pinned_again(_: &Path, stage: Stage) {
    if stage != Stage::Import {
        return;
    }
    let url = IMPORT_VISITS.with(|visits| {
        let mut visits = visits.borrow_mut();
        visits.1 += 1;
        (visits.1 == 2).then(|| visits.0.clone())
    });
    let Some(url) = url else {
        return;
    };
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut other = crate::pg_import::connect(&url).await.unwrap();
                sqlx::raw_sql("CREATE TABLE public.komika_import_state (singleton boolean PRIMARY KEY CHECK (singleton), source_identity text NOT NULL, completed boolean NOT NULL DEFAULT false, schema_hashes jsonb NOT NULL); CREATE TABLE public.komika_import_table (table_name text PRIMARY KEY, status text NOT NULL DEFAULT 'done' CHECK (status = 'done'), report jsonb NOT NULL); INSERT INTO public.komika_import_state (singleton,source_identity,schema_hashes) VALUES (true,'{\"file_size\":1}','{}')")
                    .execute(&mut other)
                    .await
                    .unwrap();
                let _ = other.close().await;
            })
    })
    .join()
    .unwrap();
}

#[tokio::test]
async fn postgres_pinned_copy_restart_happens_at_most_once_per_start() {
    with_database(|path, url| async move {
        unfinished(&path, &url).await;
        // As in E8: the record and the disk agree, PostgreSQL pinned another
        // work copy.
        let work = sibling(&path, ".import-work");
        let mut conn = SqliteConnection::connect_with(&sqlite_options(&work))
            .await
            .unwrap();
        sqlx::raw_sql("CREATE TABLE e4_pad(x); DROP TABLE e4_pad; PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let mut journal = record(&path);
        journal.work = Some(fingerprint(&work).unwrap());
        save(&sibling(&path, ".import-work-state"), &journal).unwrap();
        let current = inventory(&path).unwrap();
        IMPORT_VISITS.with(|visits| *visits.borrow_mut() = (url.clone(), 0));
        said();
        let refusal = run_path_with(
            &path,
            &url,
            Faults {
                before_stage: another_copy_is_pinned_again,
                ..Faults::NONE
            },
        )
        .await
        .unwrap_err()
        .to_string();
        // The second difference of one start is an error. Without the limit
        // the start would reset PostgreSQL a second time and go round again.
        assert_eq!(
            refusal,
            "import stage: resume source differs from the pinned source snapshot"
        );
        assert_eq!(IMPORT_VISITS.with(|visits| visits.borrow().1), 2);
        let lines = said();
        assert_eq!(count_lines(&lines, RESTARTING), 1, "{lines:?}");
        // What PostgreSQL held at the second difference is still there.
        let mut target = super::super::connect(&url).await.unwrap();
        let pinned: String =
            sqlx::query_scalar("SELECT source_identity FROM public.komika_import_state")
                .fetch_one(&mut target)
                .await
                .unwrap();
        hang_up(target).await;
        assert_eq!(pinned, "{\"file_size\":1}");
        assert_eq!(inventory(&path).unwrap(), current);
        assert_eq!(record(&path).stage, Stage::Import);
    })
    .await;
}

// ---- R5: the two warnings that matter come first ----

#[tokio::test]
async fn postgres_kept_and_smaller_warnings_precede_the_leftover_warnings() {
    with_database(|path, url| async move {
        unfinished_after(&path, &url, crate::pg_import::plan::tables().len() - 1).await;
        remove_source(&path);
        previous_release_creates_empty_database(&path).await;
        // A leftover that no record lists.
        let leftover = sibling(&resolve_source(&path).unwrap(), ".import-work-leftover");
        fs::write(&leftover, b"left by an older attempt").unwrap();
        said();
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        let place = |part: &str| {
            lines
                .iter()
                .position(|line| line.starts_with(part))
                .unwrap_or_else(|| panic!("{part} missing in {lines:?}"))
        };
        let unrecorded = format!(
            "sqlite import: warning: unrecorded file {}; leaving it alone",
            leftover.display()
        );
        // A supervisor keeps the first warnings of a start only.
        assert!(place(KEPT) < place(SMALLER), "{lines:?}");
        assert!(place(SMALLER) < place(&unrecorded), "{lines:?}");
        assert_eq!(fs::read(&leftover).unwrap(), b"left by an older attempt");
        assert_eq!(status(&url).await, Target::Complete);
    })
    .await;
}

// ---- R6: the counts of a copy whose Copy checkpoint was never written ----

#[tokio::test]
async fn postgres_copy_boundary_kill_still_gives_the_smaller_database_warning() {
    // A plain file, one with a hot rollback journal, and one with a committed
    // row that is only in the write-ahead log.
    for (hot_journal, wal) in [(false, false), (true, false), (false, true)] {
        with_database(|path, url| async move {
            if wal {
                wal_fixture(&path);
            }
            if hot_journal {
                let child = std::process::Command::new(std::env::current_exe().unwrap())
                    .arg("--exact")
                    .arg("pg_import::startup::tests::sqlite_hot_journal_fixture_child")
                    .arg("--nocapture")
                    .env("KOMIKA_STARTUP_HOT_JOURNAL_TEST_PATH", &path)
                    .status()
                    .unwrap();
                assert_eq!(child.code(), Some(86));
            }
            let original = inventory(&path).unwrap();
            assert_eq!(original.contains_key("-journal"), hot_journal);
            assert_eq!(original.contains_key("-wal"), wal);
            let what = format!("hot_journal={hot_journal} wal={wal}");
            // Killed after the snapshot and before the Copy checkpoint.
            run_path(&path, &url, Some(Stage::Copy)).await.unwrap_err();
            let journal = record(&path);
            assert_eq!(journal.stage, Stage::Copy);
            assert!(journal.rows.is_empty());
            // The previous release starts, finds its file gone and creates a
            // new, empty database.
            remove_source(&path);
            previous_release_creates_empty_database(&path).await;
            said();
            run_path(&path, &url, None).await.unwrap();
            let lines = said();
            let kept = kept_files(&path);
            assert_eq!(kept.len(), 1, "{kept:?}");
            // Counting changed no byte of the copy.
            for suffix in ["", "-journal", "-wal"] {
                assert_eq!(
                    fingerprint(&sibling(&kept[0], suffix)).ok().as_ref(),
                    original.get(suffix),
                    "{what} {suffix}"
                );
            }
            assert_eq!(count_lines(&lines, KEPT), 1, "{lines:?}");
            // Round 4 kept the copy with no counts and gave no second warning.
            let smaller = lines
                .iter()
                .find(|line| line.starts_with(SMALLER))
                .unwrap_or_else(|| panic!("{what}: {lines:?}"));
            // The row in the log is counted.
            assert!(
                smaller.contains(&kept[0].display().to_string())
                    && smaller.contains(&format!("users: 0 now, {} before", 3 + usize::from(wal))),
                "{what}: {smaller}"
            );
            assert_eq!(status(&url).await, Target::Complete);
            assert!(usernames(&url).await.is_empty());
        })
        .await;
    }
}

// ---- the staged copy is hashed again after SQLite read it ----

#[tokio::test]
async fn sqlite_snapshot_refuses_a_staged_copy_that_changed_while_it_was_read() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    wal_fixture(&path);
    let original = inventory(&path).unwrap();
    assert!(original.contains_key("-wal"));
    let journal = journal_fixture(&path, Stage::Copy);
    let log = sibling(&path, ".import-work-source-wal");
    // The snapshot says its inventory line after the staged bytes were
    // compared with the original and before SQLite opens the staged copy. At
    // that moment something appends to the staged log (not a valid frame:
    // SQLite reads the same database, but the copy is no longer exact).
    let target = log.clone();
    let mut done = false;
    on_line(Some(Box::new(move |line| {
        if !done && line.starts_with("sqlite import: source inventory start: ") {
            done = true;
            OpenOptions::new()
                .append(true)
                .open(&target)
                .unwrap()
                .write_all(&[0u8; 64])
                .unwrap();
        }
    })));
    let result = snapshot(&path, &original, &journal).await;
    on_line(None);
    assert_eq!(
        result.unwrap_err().to_string(),
        "the staged copy of the SQLite source changed while it was read"
    );
    assert_ne!(&fingerprint(&log).unwrap(), original.get("-wal").unwrap());
    assert_eq!(inventory(&path).unwrap(), original);
}

// ---- record states that the round-4 enumeration never built ----

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Later {
    /// Killed while the staged copy moved to its kept name (log moved, main
    /// file not): PostgreSQL holds our unfinished import.
    MidKeep,
    /// Killed after the reset and the cleanup: the old database is a kept
    /// copy only, PostgreSQL is empty.
    KeptOnly(Crash),
    /// PostgreSQL holds the complete import; not yet verified.
    Verify,
    /// Verified and archived; the record does not say Done yet.
    Rename,
    Done,
}

#[tokio::test]
async fn postgres_owner_never_loses_the_last_copy_in_later_record_states() {
    let cases = [
        (
            Later::MidKeep,
            FileNow::Same,
            Expected::Imported { kept: true },
        ),
        (Later::MidKeep, FileNow::Missing, Expected::Refused),
        (Later::MidKeep, FileNow::Damaged, Expected::Refused),
        (
            Later::KeptOnly(Crash::WorkDiscarded),
            FileNow::Same,
            Expected::Imported { kept: true },
        ),
        (
            Later::KeptOnly(Crash::WorkDiscarded),
            FileNow::Missing,
            Expected::Refused,
        ),
        (
            Later::KeptOnly(Crash::WorkDiscarded),
            FileNow::Damaged,
            Expected::Refused,
        ),
        (
            Later::KeptOnly(Crash::Reserved),
            FileNow::Same,
            Expected::Imported { kept: true },
        ),
        (
            Later::KeptOnly(Crash::Reserved),
            FileNow::Missing,
            Expected::Refused,
        ),
        (
            Later::KeptOnly(Crash::Reserved),
            FileNow::Damaged,
            Expected::Refused,
        ),
        (
            Later::Verify,
            FileNow::Same,
            Expected::Imported { kept: false },
        ),
        (Later::Verify, FileNow::Different, Expected::Refused),
        (Later::Verify, FileNow::Missing, Expected::Refused),
        (Later::Verify, FileNow::Damaged, Expected::Refused),
        (
            Later::Rename,
            FileNow::Same,
            Expected::Imported { kept: false },
        ),
        (
            Later::Done,
            FileNow::Same,
            Expected::Imported { kept: false },
        ),
        (
            Later::Done,
            FileNow::Different,
            Expected::Imported { kept: false },
        ),
    ];
    for (state, file, expected) in cases {
        with_database(|path, url| async move {
            let what = format!("{state:?} {file:?}");
            wal_fixture(&path);
            let original = inventory(&path).unwrap();
            // The file that the start in question is meant to import.
            let mut current = original.clone();
            match state {
                Later::MidKeep | Later::KeptOnly(_) => {
                    unfinished(&path, &url).await;
                    previous_release_writes(&path).await;
                    current = inventory(&path).unwrap();
                    killed_at(
                        &path,
                        &url,
                        match state {
                            Later::KeptOnly(point) => point,
                            _ => Crash::KeepHalfMoved,
                        },
                    )
                    .await;
                }
                Later::Verify => {
                    run_path(&path, &url, Some(Stage::Verify))
                        .await
                        .unwrap_err();
                }
                Later::Rename => {
                    run_path(&path, &url, Some(Stage::Rename))
                        .await
                        .unwrap_err();
                }
                Later::Done => run_path(&path, &url, None).await.unwrap(),
            }
            let archived_already = matches!(state, Later::Rename | Later::Done);
            match file {
                FileNow::Same => {}
                FileNow::Different if archived_already => {
                    // The previous release ran again and made a new database.
                    previous_release_creates_empty_database(&path).await;
                }
                FileNow::Different => previous_release_writes(&path).await,
                FileNow::Missing => remove_source(&path),
                FileNow::Damaged => damage_source(&path),
                _ => unreachable!(),
            }
            let owner = owner_bytes(&path);
            let was = before(&path, &url).await;
            said();
            let result = run_path(&path, &url, None).await;
            let lines = said();
            // The property: every byte of the owner that was in the
            // directory (original name, staged copy, kept sets) still is.
            assert!(
                owner.is_subset(&hashes(&path)),
                "{what}: bytes of the owner's database are gone: {result:?} {lines:?}"
            );
            match expected {
                Expected::Refused => {
                    let refusal = result.unwrap_err().to_string();
                    assert!(
                        refusal.starts_with("recovery stage: ")
                            && refusal.contains("Nothing was changed"),
                        "{what}: {refusal}"
                    );
                    assert_eq!(count_lines(&lines, RESTARTING), 0, "{what}: {lines:?}");
                    assert_eq!(count_lines(&lines, "starting with an empty"), 0, "{what}");
                    assert_eq!(count_lines(&lines, KEPT), 0, "{what}");
                    assert_nothing_changed(&path, &url, &was, &what).await;
                }
                Expected::Imported { kept } => {
                    result.unwrap_or_else(|error| panic!("{what}: {error}"));
                    assert_eq!(record(&path).stage, Stage::Done, "{what}");
                    assert_eq!(status(&url).await, Target::Complete, "{what}");
                    assert_eq!(inventory(&archived(&path)).unwrap(), current, "{what}");
                    let copies = kept_files(&path);
                    assert_eq!(copies.len(), usize::from(kept), "{what}: {copies:?}");
                    if kept {
                        // The whole set, main file and log.
                        for suffix in ["", "-wal"] {
                            assert_eq!(
                                &fingerprint(&sibling(&copies[0], suffix)).unwrap(),
                                original.get(suffix).unwrap(),
                                "{what} {suffix}"
                            );
                        }
                    }
                    if state == Later::Done && file == FileNow::Different {
                        // A file that appeared after the cutover is left alone.
                        assert_eq!(listing(&path), was.files, "{what}");
                    }
                    assert!(!sibling(&path, ".import-work").exists(), "{what}");
                }
            }
        })
        .await;
    }
}

// ===================== Fix round 6 =====================
//
// The sixth checker's findings F1 to F4 and the test gaps it named.

/// The usual source: a database in write-ahead-log mode that the previous
/// release closed cleanly. One file, no `-wal`, no `-shm`.
async fn clean_wal_source(path: &Path) {
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(path)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    sqlx::raw_sql("CREATE TABLE clean_wal_pad(x); DROP TABLE clean_wal_pad")
        .execute(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    assert_eq!(
        inventory(path).unwrap().into_keys().collect::<Vec<_>>(),
        [String::new()]
    );
    // Bytes 18 and 19 of the header: 2 is write-ahead-log mode.
    assert_eq!(fs::read(path).unwrap()[18..20], [2, 2]);
}

/// Every file in the data directory whose name says "kept copy", found by
/// name alone (no product function).
fn kept_names(path: &Path) -> Vec<String> {
    listing(path)
        .into_keys()
        .filter(|name| name.contains(".import-kept-"))
        .collect()
}

fn work_names(path: &Path) -> Vec<String> {
    listing(path)
        .into_keys()
        .filter(|name| name.contains(".import-work") && !name.ends_with("-state"))
        .collect()
}

// ---- F1: what SQLite leaves beside a staged copy is not the owner's data ----

#[tokio::test]
async fn postgres_clean_wal_source_restarted_unchanged_keeps_no_copy() {
    // A kill at every stage that has a staged copy, and a restart with the
    // file unchanged. Before the import began PostgreSQL is empty, so the
    // next start begins again; at the Import stage a changed binary does it.
    for stop in [
        Stage::Copy,
        Stage::Migrate,
        Stage::Repair,
        Stage::Import,
        Stage::Verify,
        Stage::Rename,
    ] {
        with_database(|path, url| async move {
            clean_wal_source(&path).await;
            let original = inventory(&path).unwrap();
            if stop == Stage::Import {
                unfinished(&path, &url).await;
                let mut journal = record(&path);
                journal.prepared_migrations = "the migrations of an older release".into();
                save(&sibling(&path, ".import-work-state"), &journal).unwrap();
            } else {
                run_path(&path, &url, Some(stop)).await.unwrap_err();
            }
            let staged = staged_file(&path);
            if stop != Stage::Rename {
                assert_eq!(&fingerprint(&staged).unwrap(), original.get("").unwrap());
            }
            said();
            run_path(&path, &url, None)
                .await
                .unwrap_or_else(|error| panic!("{stop:?}: {error}"));
            let lines = said();
            // Round 5: "the copy ... differs from the file now at ..." and a
            // full-size duplicate, for a file that had not changed, because
            // SQLite's read-only open left an empty -wal beside the staged copy.
            assert_eq!(count_lines(&lines, KEPT), 0, "{stop:?}: {lines:?}");
            assert_eq!(count_lines(&lines, SMALLER), 0, "{stop:?}: {lines:?}");
            assert_eq!(kept_names(&path), Vec::<String>::new(), "{stop:?}");
            assert!(record(&path).kept.is_none(), "{stop:?}");
            assert_eq!(
                count_lines(&lines, RESTARTING),
                usize::from(!matches!(stop, Stage::Verify | Stage::Rename)),
                "{stop:?}: {lines:?}"
            );
            assert_eq!(status(&url).await, Target::Complete, "{stop:?}");
            assert_eq!(inventory(&archived(&path)).unwrap(), original, "{stop:?}");
            assert_eq!(work_names(&path), Vec::<String>::new(), "{stop:?}");
        })
        .await;
    }
    // The same source, changed for real: the copy is kept, and it is called
    // what it is. What SQLite left beside the staged copy is not part of it.
    with_database(|path, url| async move {
        clean_wal_source(&path).await;
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        let current = inventory(&path).unwrap();
        said();
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        let kept = kept_files(&path);
        assert_eq!(kept.len(), 1, "{kept:?}");
        assert_eq!(&fingerprint(&kept[0]).unwrap(), original.get("").unwrap());
        assert_eq!(
            kept_names(&path),
            [kept[0].file_name().unwrap().to_string_lossy().into_owned()]
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with(KEPT))
                .collect::<Vec<_>>(),
            [&format!(
                "{KEPT}{}; an exact copy of the database as it was then is kept at {}. The server never deletes it: you may delete it once you have checked your data",
                resolve_source(&path).unwrap().display(),
                kept[0].display()
            )],
            "{lines:?}"
        );
        let recorded = record(&path).kept.unwrap();
        assert!(recorded.moved && recorded.complete);
        assert_eq!(recorded.files.into_keys().collect::<Vec<_>>(), [String::new()]);
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), current);
        assert_eq!(work_names(&path), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn staged_sidecar_counts_only_when_it_can_hold_the_owners_data() {
    let dir = Directory::new();
    let path = dir.source();
    golden(&path).await;
    clean_wal_source(&path).await;
    let state = sibling(&path, ".import-work-state");
    let staged = sibling(&path, ".import-work-source");
    let journal = journal_fixture(&path, Stage::Import);
    save(&state, &journal).unwrap();
    fs::copy(&path, &staged).unwrap();
    let main = fingerprint(&staged).unwrap();
    for suffix in ["-wal", "-journal"] {
        let sidecar = sibling(&staged, suffix);
        // Empty, and the original had no such file: SQLite made it when it
        // read the copy. The copy is exact and has its twin.
        fs::write(&sidecar, b"").unwrap();
        assert_eq!(
            staged_with(&path, &journal).unwrap(),
            (
                Staged::Copy { complete: true },
                BTreeMap::from([(String::new(), main.clone())])
            ),
            "{suffix}"
        );
        assert!(staged_is_redundant(&path, &journal).unwrap(), "{suffix}");
        disposable_staged(&path, &journal).unwrap();
        let mut same = journal.clone();
        said();
        protect_staged(&path, &state, &mut same, None).unwrap();
        assert!(same.kept.is_none() && staged.exists(), "{suffix}");
        assert_eq!(said(), Vec::<String>::new(), "{suffix}");
        // A record that has no original yet rules nothing out.
        let mut blank = journal.clone();
        blank.original.clear();
        assert!(
            left_by_sqlite(&sidecar, suffix, &journal).unwrap(),
            "{suffix}"
        );
        assert!(
            !left_by_sqlite(&sidecar, suffix, &blank).unwrap(),
            "{suffix}"
        );
        // One byte in it: it may be the owner's log, and it has no twin.
        fs::write(&sidecar, b"x").unwrap();
        let (kind, files) = staged_with(&path, &journal).unwrap();
        assert_eq!(kind, Staged::Copy { complete: false }, "{suffix}");
        assert_eq!(
            files.keys().cloned().collect::<Vec<_>>(),
            [String::new(), suffix.to_owned()],
            "{suffix}"
        );
        assert!(!staged_is_redundant(&path, &journal).unwrap(), "{suffix}");
        assert!(disposable_staged(&path, &journal).is_err(), "{suffix}");
        // Empty, but the original had one (also empty): a recorded component
        // like any other, and its twin is required.
        fs::write(&sidecar, b"").unwrap();
        fs::write(sibling(&path, suffix), b"").unwrap();
        let recorded = journal_fixture(&path, Stage::Import);
        assert!(recorded.original.contains_key(suffix), "{suffix}");
        let (kind, files) = staged_with(&path, &recorded).unwrap();
        assert_eq!(kind, Staged::Copy { complete: true }, "{suffix}");
        assert!(files.contains_key(suffix), "{suffix}");
        fs::remove_file(sibling(&path, suffix)).unwrap();
        assert!(!staged_is_redundant(&path, &recorded).unwrap(), "{suffix}");
        fs::remove_file(&sidecar).unwrap();
    }
    // A copy that is really kept leaves SQLite's empty file behind at the
    // staged name; it gets no kept name and is in no list.
    fs::write(sibling(&staged, "-wal"), b"").unwrap();
    previous_release_writes(&path).await;
    let mut kept = journal.clone();
    said();
    protect_staged(&path, &state, &mut kept, None).unwrap();
    let record = kept.kept.clone().unwrap();
    let name = kept_path(&path, &record.stamp).unwrap();
    assert!(record.moved && record.complete);
    assert_eq!(
        record.files,
        BTreeMap::from([(String::new(), main.clone())])
    );
    assert_eq!(fingerprint(&name).unwrap(), main);
    assert!(!sibling(&name, "-wal").exists() && sibling(&staged, "-wal").exists());
    let lines = said();
    assert!(
        lines.len() == 1
            && lines[0].contains(&format!(
                "an exact copy of the database as it was then is kept at {}. The server",
                name.display()
            )),
        "{lines:?}"
    );
    // With the proof in hand it goes with the other work files.
    discard_work(&path, &kept, Some(Disposable(()))).unwrap();
    assert!(!sibling(&staged, "-wal").exists());
    assert_eq!(fingerprint(&name).unwrap(), main);
}

// ---- F2: no printed rename lands on a name that is taken ----

#[tokio::test]
async fn postgres_printed_renames_never_land_on_an_occupied_name() {
    // A damaged main file with a write-ahead log beside it, and a staged copy
    // that has a log of its own. Round 5 said "move it out of the data
    // directory" (the main file) and then renamed the copy's log onto the
    // log that was still there.
    with_database(|path, url| async move {
        wal_fixture(&path);
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        assert!(sibling(&staged, "-wal").exists());
        damage_source(&path);
        let log = sibling(&source, "-wal");
        fs::write(&log, b"a log that no rename may replace").unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "(if the file is damaged: move {}, {} out of the data directory, then rename {} to {}, {} to {}) and start again",
                source.display(),
                log.display(),
                staged.display(),
                source.display(),
                sibling(&staged, "-wal").display(),
                log.display()
            )),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "damaged file with a log").await;
        // To the letter. A rename onto a taken name fails in `follow_renames`.
        let out = follow_renames(&refusal, "if the file is damaged: ");
        assert_eq!(
            fs::read(out.0.join(log.file_name().unwrap())).unwrap(),
            b"a log that no rename may replace"
        );
        assert_eq!(
            fs::read(out.0.join(source.file_name().unwrap())).unwrap(),
            vec![0u8; 4096]
        );
        for suffix in ["", "-wal"] {
            assert_eq!(
                &fingerprint(&sibling(&path, suffix)).unwrap(),
                original.get(suffix).unwrap()
            );
        }
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        assert!(usernames(&url).await.contains(&"wal-owner".to_owned()));
    })
    .await;
    // The database exists as a kept copy only, and a log without its main
    // file is at the original name. SQLite would read that log as a part of
    // the kept copy once the copy has the name.
    for with_record in [true, false] {
        with_database(|path, url| async move {
            let what = format!("record={with_record}");
            let original = inventory(&path).unwrap();
            unfinished(&path, &url).await;
            previous_release_writes(&path).await;
            killed_at(&path, &url, Crash::WorkDiscarded).await;
            let source = resolve_source(&path).unwrap();
            let kept = kept_files(&path);
            assert_eq!(kept.len(), 1, "{what}");
            let holds = facts_of(&kept[0]).await;
            let elsewhere = Directory::new();
            for suffix in COMPONENTS {
                let _ = fs::rename(sibling(&path, suffix), sibling(&elsewhere.source(), suffix));
            }
            let log = sibling(&source, "-wal");
            fs::write(&log, b"a stale log").unwrap();
            if !with_record {
                fs::remove_file(sibling(&source, ".import-work-state")).unwrap();
            }
            let was = before(&path, &url).await;
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            let steps = format!(
                "move {} out of the data directory, then rename {} to {}",
                log.display(),
                kept[0].display(),
                source.display()
            );
            assert!(
                refusal.contains(&if with_record {
                    format!("(if the file is damaged: {steps}) and start again")
                } else {
                    format!(
                        "To import the newest one, {} ({holds}), {steps} and start again.",
                        kept[0].display()
                    )
                }),
                "{what}: {refusal}"
            );
            assert_nothing_changed(&path, &url, &was, &what).await;
            let out = follow_renames(
                &refusal,
                if with_record {
                    "if the file is damaged: "
                } else {
                    "To import the newest one, "
                },
            );
            assert_eq!(
                fs::read(out.0.join(log.file_name().unwrap())).unwrap(),
                b"a stale log",
                "{what}"
            );
            assert!(!log.exists(), "{what}");
            run_path(&path, &url, None).await.unwrap();
            assert_eq!(status(&url).await, Target::Complete, "{what}");
            assert_eq!(
                inventory(&archived(&path)).unwrap().get(""),
                original.get(""),
                "{what}"
            );
            assert!(!usernames(&url).await.contains(&"rollback-user".to_owned()));
        })
        .await;
    }
    // A copy whose move aside stopped half-way, and a damaged file at the
    // original name: the renames put back both pieces, after the file left.
    with_database(|path, url| async move {
        wal_fixture(&path);
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        let kept = record(&path).kept.unwrap();
        let log = sibling(&kept_path(&source, &kept.stamp).unwrap(), "-wal");
        damage_source(&path);
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "(if the file is damaged: move {} out of the data directory, then rename {} to {}, {} to {}) and start again",
                source.display(),
                staged.display(),
                source.display(),
                log.display(),
                sibling(&source, "-wal").display()
            )),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "half-moved copy, damaged file").await;
        let out = follow_renames(&refusal, "if the file is damaged: ");
        assert_eq!(
            fs::read(out.0.join(source.file_name().unwrap())).unwrap(),
            vec![0u8; 4096]
        );
        for suffix in ["", "-wal"] {
            assert_eq!(
                &fingerprint(&sibling(&path, suffix)).unwrap(),
                original.get(suffix).unwrap()
            );
        }
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        assert!(usernames(&url).await.contains(&"wal-owner".to_owned()));
    })
    .await;
    // No record and no file, a staged copy, and a log without its main file
    // at the original name.
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        assert_eq!(status(&url).await, Target::Empty);
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        fs::remove_file(sibling(&source, ".import-work-state")).unwrap();
        remove_source(&path);
        let log = sibling(&source, "-wal");
        fs::write(&log, b"a stale log").unwrap();
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "To import it, move {} out of the data directory, then rename {} to {}, delete ",
                log.display(),
                staged.display(),
                source.display()
            )),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "no record, a stale log").await;
        let out = follow_renames(&refusal, "To import it, ");
        assert_eq!(
            fs::read(out.0.join(log.file_name().unwrap())).unwrap(),
            b"a stale log"
        );
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
    })
    .await;
}

#[test]
fn clear_first_names_every_file_at_the_original_name() {
    let dir = Directory::new();
    let path = dir.source();
    assert_eq!(clear_first(&path), "");
    for suffix in ["-journal", "", "-shm", "-wal"] {
        fs::write(sibling(&path, suffix), b"x").unwrap();
    }
    // Every one of them, also the `-shm` that is no data: beside a renamed
    // main file SQLite would take it for that file's.
    assert_eq!(
        clear_first(&path),
        format!(
            "move {}, {}, {}, {} out of the data directory, then ",
            path.display(),
            sibling(&path, "-wal").display(),
            sibling(&path, "-shm").display(),
            sibling(&path, "-journal").display()
        )
    );
    fs::remove_file(&path).unwrap();
    fs::remove_file(sibling(&path, "-wal")).unwrap();
    fs::remove_file(sibling(&path, "-journal")).unwrap();
    assert_eq!(
        clear_first(&path),
        format!(
            "move {} out of the data directory, then ",
            sibling(&path, "-shm").display()
        )
    );
}

// ---- F3: a kept copy is offered with what it holds ----

/// What a kept main file holds, in the words of a refusal. Counted here with
/// plain SQL on a copy of the file in another directory, not by the product.
async fn facts_of(file: &Path) -> String {
    let elsewhere = Directory::new();
    let copy = elsewhere.source();
    fs::copy(file, &copy).unwrap();
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(&copy)
        .disable_statement_logging();
    let mut conn = SqliteConnection::connect_with(&options).await.unwrap();
    let mut counts = Vec::new();
    for table in ["comments", "user_library", "users", "work"] {
        let count: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut conn)
            .await
            .unwrap();
        counts.push(format!("{table}: {count}"));
    }
    let empty = counts.iter().all(|count| count.ends_with(": 0"));
    conn.close().await.unwrap();
    if empty {
        EMPTY_COPY.to_owned()
    } else {
        counts.join(", ")
    }
}

const EMPTY_COPY: &str = "empty: no rows in users, user_library, comments, work";
const UNCOUNTED_COPY: &str = "rows not counted: it has a write-ahead log or a rollback journal";
const SCAN: &str = "sqlite import: source inventory start: ";

#[tokio::test]
async fn postgres_kept_copies_are_offered_with_what_they_hold() {
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        killed_at(&path, &url, Crash::WorkDiscarded).await;
        assert_eq!(status(&url).await, Target::Empty);
        let source = resolve_source(&path).unwrap();
        let state = sibling(&source, ".import-work-state");
        // The set that this record made: the owner's data.
        let recorded = kept_files(&path);
        assert_eq!(recorded.len(), 1);
        let data = recorded[0].clone();
        let holds = facts_of(&data).await;
        assert!(
            holds.starts_with("comments: ") && !holds.contains("users: 0"),
            "{holds}"
        );
        // A newer set: the empty database that the previous release creates
        // when it finds no file (write-ahead-log mode, one file).
        let newest = sibling(&source, ".import-kept-20991231T235959Z");
        previous_release_creates_empty_database(&newest).await;
        assert!(!sibling(&newest, "-wal").exists() && !sibling(&newest, "-shm").exists());
        assert_eq!(fs::read(&newest).unwrap()[18..20], [2, 2]);
        assert_eq!(facts_of(&newest).await, EMPTY_COPY);
        // An older set with a write-ahead log.
        let oldest = sibling(&source, ".import-kept-20000101T000000Z");
        fs::copy(&data, &oldest).unwrap();
        fs::write(sibling(&oldest, "-wal"), b"a log").unwrap();
        let elsewhere = Directory::new();
        for suffix in COMPONENTS {
            let _ = fs::rename(sibling(&path, suffix), sibling(&elsewhere.source(), suffix));
        }
        // With the record. Round 5 offered "the kept copy" with no word about
        // what it holds, and named the other sets as bare file names.
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                " Kept by earlier starts (each is the SQLite database as it was when an earlier import attempt began): {} ({UNCOUNTED_COPY}), {}, {} ({holds}), {} ({EMPTY_COPY}).",
                oldest.display(),
                sibling(&oldest, "-wal").display(),
                data.display(),
                newest.display()
            )) && refusal.contains(&format!(
                " Or, to import the kept copy {} ({holds}) instead, rename {} to {} and start again; any other kept copy is imported the same way.",
                data.display(),
                data.display(),
                source.display()
            )),
            "{refusal}"
        );
        // One file is read to be counted, and the line that says so comes
        // first, after the first line of every start that reads files.
        // Counting creates no file beside a kept copy.
        let lines = said();
        assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with(SCAN))
                .collect::<Vec<_>>(),
            [&format!(
                "{SCAN}{} bytes",
                fs::metadata(&newest).unwrap().len()
            )],
            "{lines:?}"
        );
        assert_nothing_changed(&path, &url, &was, "kept copies, with the record").await;
        // Without the record. Round 5: "To import the newest one, rename
        // <the empty database> ...", and not a word that it is empty.
        fs::remove_file(&state).unwrap();
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "To import the newest one, {} ({EMPTY_COPY}), rename {} to {} and start again. The other kept copies, which are imported the same way: {} ({UNCOUNTED_COPY}), {} ({holds}). Or, to start with an empty database, move the listed files out of the data directory and start again",
                newest.display(),
                newest.display(),
                source.display(),
                oldest.display(),
                data.display()
            )),
            "{refusal}"
        );
        // Two files are read now, each after its own line.
        let lines = said();
        assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.starts_with(SCAN))
                .collect::<Vec<_>>(),
            [
                &format!("{SCAN}{} bytes", fs::metadata(&data).unwrap().len()),
                &format!("{SCAN}{} bytes", fs::metadata(&newest).unwrap().len())
            ],
            "{lines:?}"
        );
        assert_nothing_changed(&path, &url, &was, "kept copies, no record").await;
        // The owner picks the set that holds the data, the same way.
        let others = listing(&path);
        fs::rename(&data, &source).unwrap();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap().get(""), original.get(""));
        // The other kept sets are as they were.
        let now = listing(&path);
        for file in [&oldest, &sibling(&oldest, "-wal"), &newest] {
            let name = file.file_name().unwrap().to_string_lossy().into_owned();
            assert_eq!(now.get(&name), others.get(&name), "{name}");
        }
        assert_eq!(kept_names(&path).len(), 3);
    })
    .await;
}

#[tokio::test]
async fn kept_copy_that_is_no_database_says_so_and_is_left_alone() {
    let dir = Directory::new();
    let path = dir.source();
    let kept = kept_path(&path, "20261011T123456Z").unwrap();
    fs::write(&kept, b"not a database at all").unwrap();
    let files = listing(&path);
    said();
    let facts = kept_facts(&path, None, std::slice::from_ref(&kept)).await;
    assert_eq!(
        facts,
        [(
            kept.clone(),
            "rows not counted: it could not be read as a database".to_owned()
        )]
    );
    assert_eq!(said(), [format!("{SCAN}21 bytes")]);
    assert_eq!(listing(&path), files);
    // A rollback journal beside it: not counted either, and not opened.
    fs::write(sibling(&kept, "-journal"), b"a journal").unwrap();
    let files = listing(&path);
    let facts = kept_facts(&path, None, &[kept.clone(), sibling(&kept, "-journal")]).await;
    assert_eq!(facts, [(kept.clone(), UNCOUNTED_COPY.to_owned())]);
    assert_eq!(said(), Vec::<String>::new());
    assert_eq!(listing(&path), files);
}

// ---- the gaps that the sixth checker named ----

#[test]
fn interrupted_keep_offers_the_way_back_only_onto_free_names() {
    for (suffix, name) in [
        ("-wal", "the write-ahead log"),
        ("-journal", "the rollback journal"),
    ] {
        let dir = Directory::new();
        let path = dir.source();
        fs::write(&path, b"another database").unwrap();
        let state = sibling(&path, ".import-work-state");
        let staged = sibling(&path, ".import-work-source");
        let stamp = "20261011T123456Z";
        let kept = kept_path(&path, stamp).unwrap();
        fs::write(&staged, b"the original").unwrap();
        fs::write(sibling(&staged, suffix), b"its sidecar").unwrap();
        let mut journal = journal_fixture(&path, Stage::Import);
        journal.kept = Some(Kept {
            stamp: stamp.into(),
            moved: false,
            complete: true,
            rows: BTreeMap::new(),
            files: BTreeMap::from([
                (String::new(), fingerprint(&staged).unwrap()),
                (
                    suffix.to_owned(),
                    fingerprint(&sibling(&staged, suffix)).unwrap(),
                ),
            ]),
        });
        save(&state, &journal).unwrap();
        // The sidecar has its kept name, and the owner put the main file back
        // at the original name.
        let aside = sibling(&kept, suffix);
        let back = sibling(&path, suffix);
        fs::rename(sibling(&staged, suffix), &aside).unwrap();
        fs::rename(&staged, &path).unwrap();
        let refuse = |journal: &mut Journal| {
            protect_staged(&path, &state, journal, None)
                .err()
                .unwrap()
                .to_string()
        };
        let refusal = refuse(&mut journal);
        assert!(
            refusal.contains(&format!(
                "that copy is not complete now: the main file was put back at {}; {name} is at {}.",
                path.display(),
                aside.display()
            )) && refusal.contains(&format!(
                "To finish putting that database back, rename {} to {} and start again.",
                aside.display(),
                back.display()
            )),
            "{suffix}: {refusal}"
        );
        // Something has that name now (the log of the file that is in use):
        // no rename onto it is printed.
        fs::write(&back, b"in use").unwrap();
        let refusal = refuse(&mut journal);
        assert!(
            !refusal.contains("To finish putting")
                && !refusal.contains(&format!("rename {}", aside.display()))
                && refusal.contains(&format!(
                    "To go on without that copy, move {} out of the data directory",
                    aside.display()
                )),
            "{suffix}: {refusal}"
        );
        assert_eq!(fs::read(&back).unwrap(), b"in use");
        assert_eq!(fs::read(&aside).unwrap(), b"its sidecar");
        assert!(!record(&path).kept.unwrap().moved);
        // Free again: the printed rename puts the piece back.
        fs::remove_file(&back).unwrap();
        let refusal = refuse(&mut journal);
        follow_renames(&refusal, "To finish putting that database back, ");
        assert_eq!(fs::read(&back).unwrap(), b"its sidecar");
        assert_eq!(fs::read(&path).unwrap(), b"the original");
        said();
        protect_staged(&path, &state, &mut journal, None).unwrap();
        assert!(record(&path).kept.is_none());
        assert_eq!(count_lines(&said(), KEPT), 0);
    }
}

#[tokio::test]
async fn postgres_kept_sidecar_without_its_main_file_stops_the_empty_start() {
    for with_record in [true, false] {
        with_database(|path, url| async move {
            let what = format!("record={with_record}");
            wal_fixture(&path);
            let original = inventory(&path).unwrap();
            unfinished(&path, &url).await;
            previous_release_writes(&path).await;
            killed_at(&path, &url, Crash::WorkDiscarded).await;
            assert_eq!(status(&url).await, Target::Empty, "{what}");
            let source = resolve_source(&path).unwrap();
            let state = sibling(&source, ".import-work-state");
            let kept = kept_files(&path);
            assert_eq!(kept.len(), 1, "{what}");
            let log = sibling(&kept[0], "-wal");
            assert_eq!(&fingerprint(&log).unwrap(), original.get("-wal").unwrap());
            // Of the kept set only the log is left, and the file is gone.
            let elsewhere = Directory::new();
            fs::rename(&kept[0], elsewhere.0.join("kept")).unwrap();
            for suffix in COMPONENTS {
                let _ = fs::rename(sibling(&path, suffix), sibling(&elsewhere.source(), suffix));
            }
            if !with_record {
                fs::remove_file(&state).unwrap();
            }
            assert_eq!(
                kept_names(&path),
                [log.file_name().unwrap().to_string_lossy().into_owned()]
            );
            let was = before(&path, &url).await;
            said();
            // A committed row may be in that log only: no empty start, no
            // marker, and no rename of a log without its main file.
            let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
            assert!(
                refusal.contains("is missing")
                    && refusal.contains("Nothing was changed")
                    && refusal.contains(&log.display().to_string())
                    && !refusal.contains(&format!("rename {}", log.display()))
                    && refusal.contains(&format!(
                        "move {} out of the data directory",
                        if with_record {
                            log.display().to_string()
                        } else {
                            "the listed files".to_owned()
                        }
                    )),
                "{what}: {refusal}"
            );
            assert_eq!(
                count_lines(&said(), "starting with an empty database"),
                0,
                "{what}"
            );
            assert_nothing_changed(&path, &url, &was, &what).await;
            // The way out, as written.
            if with_record {
                fs::remove_file(&state).unwrap();
            }
            fs::rename(&log, elsewhere.0.join("kept-wal")).unwrap();
            run_path(&path, &url, None).await.unwrap();
            assert!(started_marker(&url).await, "{what}");
        })
        .await;
    }
}

#[tokio::test]
async fn postgres_half_moved_piece_with_other_bytes_is_not_offered_as_the_way_back() {
    with_database(|path, url| async move {
        wal_fixture(&path);
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        // The log has its kept name, the main file is still staged.
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        let kept = record(&path).kept.unwrap();
        let log = sibling(&kept_path(&source, &kept.stamp).unwrap(), "-wal");
        assert!(kept.files.contains_key("-wal") && log.exists() && staged.exists());
        // Another file has the kept name of the log now.
        fs::write(&log, b"not the log that was recorded").unwrap();
        remove_source(&path);
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "A part of the copy of the database as it was when the import began was being moved aside when an earlier start stopped, and is now at these names: the main file at {}; the write-ahead log at {} (not the bytes that were recorded).",
                staged.display(),
                log.display()
            )) && refusal.contains(&format!(
                "To continue, put {} back and start again.",
                source.display()
            )) && !refusal.contains(&format!("{} to ", log.display())),
            "{refusal}"
        );
        // The piece is hashed, and the line that says so comes first; the
        // first line of all is the one of every start that reads files.
        let lines = said();
        assert_eq!(
            lines,
            [PREPARING.to_owned(), format!("{SCAN}29 bytes")],
            "{lines:?}"
        );
        assert_nothing_changed(&path, &url, &was, "a foreign piece").await;
    })
    .await;
    // The owner put the log back by hand and not the main file: the file at
    // the original name of the log is hashed to see whether it is the piece.
    // The first line comes before that too, never after a scan line.
    with_database(|path, url| async move {
        wal_fixture(&path);
        let original = inventory(&path).unwrap();
        unfinished(&path, &url).await;
        previous_release_writes(&path).await;
        killed_at(&path, &url, Crash::KeepHalfMoved).await;
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        let kept = record(&path).kept.unwrap();
        let log = sibling(&kept_path(&source, &kept.stamp).unwrap(), "-wal");
        remove_source(&path);
        let back = sibling(&source, "-wal");
        fs::rename(&log, &back).unwrap();
        let was = before(&path, &url).await;
        said();
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        assert!(
            refusal.contains(&format!(
                "A part of the copy of the database as it was when the import began was being moved aside when an earlier start stopped, and is now at these names: the main file at {}; the write-ahead log put back at {}.",
                staged.display(),
                back.display()
            )),
            "{refusal}"
        );
        assert_eq!(
            said(),
            [
                PREPARING.to_owned(),
                format!("{SCAN}{} bytes", original.get("-wal").unwrap().bytes)
            ]
        );
        assert_nothing_changed(&path, &url, &was, "the log put back by hand").await;
    })
    .await;
}

// ---- a start that prints work lines and ends well ends with "complete" ----

#[tokio::test]
async fn postgres_finished_archive_of_a_done_record_ends_with_the_complete_line() {
    with_database(|path, url| async move {
        let original = inventory(&path).unwrap();
        run_path(&path, &url, Some(Stage::Rename))
            .await
            .unwrap_err();
        // Done was published, its directory sync failed, the rename was undone.
        let state = sibling(&path, ".import-work-state");
        let mut journal = record(&path);
        journal.stage = Stage::Done;
        assert!(
            save_with_sync(&state, &journal, |_| anyhow::bail!("injected"))
                .unwrap_err()
                .published
        );
        rollback_archive(&path, &journal).unwrap();
        assert_eq!(inventory(&path).unwrap(), original);
        said();
        // This start verifies again and finishes the rename. Round 5 printed
        // the verify and rename lines and then stopped talking: a reader of
        // the lines could not tell that the cutover had ended well.
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(lines.first().map(String::as_str), Some(PREPARING));
        for part in [
            "sqlite import: verify start",
            "sqlite import: verifier PASS: ",
            "sqlite import: verify complete: ",
            "sqlite import: rename start",
            "sqlite import: renamed original to ",
            "sqlite import: rename complete: ",
            "sqlite import: complete: ",
        ] {
            assert_eq!(count_lines(&lines, part), 1, "{part}: {lines:?}");
        }
        let last = lines.last().unwrap();
        assert!(
            last.starts_with("sqlite import: complete: ") && last.ends_with("s total"),
            "{lines:?}"
        );
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        assert!(!path.exists());
        assert_eq!(status(&url).await, Target::Complete);
        // Later starts have nothing to say.
        said();
        run_path(&path, &url, None).await.unwrap();
        assert_eq!(said(), Vec::<String>::new());
    })
    .await;
}

#[tokio::test]
async fn postgres_clean_wal_source_lost_is_put_back_by_one_rename() {
    with_database(|path, url| async move {
        clean_wal_source(&path).await;
        let original = inventory(&path).unwrap();
        run_path(&path, &url, Some(Stage::Repair))
            .await
            .unwrap_err();
        let source = resolve_source(&path).unwrap();
        let staged = staged_file(&path);
        // SQLite left an empty log beside the copy that it read.
        let scratch = sibling(&staged, "-wal");
        assert_eq!(fs::metadata(&scratch).unwrap().len(), 0);
        remove_source(&path);
        let was = before(&path, &url).await;
        let refusal = run_path(&path, &url, None).await.unwrap_err().to_string();
        // The copy is exact and it is one file. Round 5 called it "a copy of
        // the main file (without its complete write-ahead log)" and told the
        // owner to rename the empty log to the original name as well.
        assert!(
            refusal.contains(&format!(
                "An exact copy of the database as it was when the import began is at {}.",
                staged.display()
            )) && refusal.contains(&format!(
                "To continue, rename {} to {} and start again.",
                staged.display(),
                source.display()
            )) && !refusal.contains(&format!("{} to ", scratch.display())),
            "{refusal}"
        );
        assert_nothing_changed(&path, &url, &was, "one-file source lost").await;
        follow_renames(&refusal, "To continue, ");
        assert_eq!(inventory(&path).unwrap(), original);
        said();
        run_path(&path, &url, None).await.unwrap();
        let lines = said();
        assert_eq!(count_lines(&lines, KEPT), 0, "{lines:?}");
        assert_eq!(status(&url).await, Target::Complete);
        assert_eq!(inventory(&archived(&path)).unwrap(), original);
        // What SQLite left went with the work files.
        assert_eq!(work_names(&path), Vec::<String>::new());
        assert_eq!(kept_names(&path), Vec::<String>::new());
    })
    .await;
}
