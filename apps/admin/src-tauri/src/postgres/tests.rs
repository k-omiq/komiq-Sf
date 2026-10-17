use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
static NEXT: AtomicUsize = AtomicUsize::new(0);
struct Scratch(PathBuf);
impl Scratch {
  fn new() -> Self {
    let path = std::env::temp_dir().join(format!(
      "komiq-d-pure-{}-{} é 漢字",
      std::process::id(),
      NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir_all(&path).unwrap();
    Self(path)
  }
}
impl Drop for Scratch {
  fn drop(&mut self) {
    let _ = fs::remove_dir_all(&self.0);
  }
}

#[test]
fn credentials_are_stable_private_and_distinct() {
  let root = Scratch::new();
  let first = Credentials::load(&root.0, false).unwrap();
  let second = Credentials::load(&root.0, true).unwrap();
  assert_eq!(first.supervisor, second.supervisor);
  assert_eq!(first.application, second.application);
  assert_ne!(first.supervisor, first.application);
  assert!(first.supervisor.len() >= 32);
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
      fs::metadata(root.0.join("credentials"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777,
      0o600
    );
  }
}

#[test]
fn missing_credentials_never_regenerated_for_existing_data() {
  let root = Scratch::new();
  assert!(Credentials::load(&root.0, true)
    .err()
    .unwrap()
    .contains("Restore"));
  assert!(!root.0.join("credentials").exists());
}

#[test]
fn corrupt_credentials_are_preserved() {
  let root = Scratch::new();
  let path = root.0.join("credentials");
  private_new(&path).unwrap().write_all(b"broken").unwrap();
  assert!(Credentials::load(&root.0, false).is_err());
  assert_eq!(fs::read(&path).unwrap(), b"broken");
}

#[cfg(unix)]
#[test]
fn unsafe_credentials_permissions_are_refused() {
  use std::os::unix::fs::PermissionsExt;
  let root = Scratch::new();
  Credentials::load(&root.0, false).unwrap();
  fs::set_permissions(
    root.0.join("credentials"),
    fs::Permissions::from_mode(0o644),
  )
  .unwrap();
  assert!(Credentials::load(&root.0, true).is_err());
}

#[test]
fn urls_encode_reserved_characters_and_utf8() {
  assert_eq!(encode("a:@/ ?é漢"), "a%3A%40%2F%20%3F%C3%A9%E6%BC%A2");
  assert_eq!(encode("a-z_A.0~"), "a-z_A.0~");
  let credentials = Credentials {
    supervisor: "s".repeat(32),
    application: "a:@/".into(),
  };
  assert_eq!(
    credentials.url(1234),
    "postgres://komika_app:a%3A%40%2F@127.0.0.1:1234/komika"
  );
}

#[test]
fn passwords_and_urls_mask_before_output() {
  let log = LogBuf::new();
  let credentials = Credentials {
    supervisor: "s".repeat(32),
    application: "a".repeat(32),
  };
  credentials.mask(std::slice::from_ref(&log));
  log.add_secret(&credentials.url(2345));
  log.push(&format!(
    "{} {} {}",
    credentials.supervisor,
    credentials.application,
    credentials.url(2345)
  ));
  assert!(!log.text().contains(&credentials.supervisor));
  assert!(!log.text().contains(&credentials.application));
}

#[test]
fn port_retry_requires_collision_and_has_five_attempt_limit() {
  for attempt in 0..4 {
    assert!(retry_port(
      attempt,
      "could not bind: Address already in use"
    ));
  }
  assert!(!retry_port(4, "Address already in use"));
  for cause in ["disk full", "Permission denied", "database corrupt", ""] {
    assert!(!retry_port(0, cause));
  }
}

#[test]
fn restart_budget_is_independent_and_backed_off() {
  let now = Instant::now();
  let mut budget = RestartBudget::default();
  assert_eq!(budget.record(now), Some(Duration::from_secs(2)));
  assert_eq!(budget.record(now), Some(Duration::from_secs(4)));
  assert_eq!(budget.record(now), Some(Duration::from_secs(8)));
  assert_eq!(budget.record(now), None);
  assert_eq!(
    budget.record(now + Duration::from_secs(61)),
    Some(Duration::from_secs(2))
  );
}

#[test]
fn ownership_lock_excludes_second_instance_and_releases_on_close() {
  let root = Scratch::new();
  let guard = Ownership::acquire(&root.0).unwrap();
  assert!(Ownership::acquire(&root.0)
    .err()
    .unwrap()
    .contains(&format!("pid {}", std::process::id())));
  drop(guard);
  // Another test thread that starts a program at this moment holds a copy of every open
  // file of this process until its child runs its own program: the lock with it.
  let deadline = Instant::now() + Duration::from_secs(5);
  while Ownership::acquire(&root.0).is_err() && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(10));
  }
  assert!(Ownership::acquire(&root.0).is_ok());
}

#[test]
fn incomplete_and_foreign_major_clusters_are_preserved() {
  let root = Scratch::new();
  fs::write(root.0.join("precious"), "data").unwrap();
  assert!(check_cluster(&root.0)
    .err()
    .unwrap()
    .contains("Partly initialized"));
  fs::write(root.0.join("PG_VERSION"), "17\n").unwrap();
  assert!(check_cluster(&root.0)
    .err()
    .unwrap()
    .contains("was made by PostgreSQL 17;"));
  assert_eq!(fs::read_to_string(root.0.join("precious")).unwrap(), "data");
}

fn fake_manifest(root: &Path) -> serde_json::Value {
  let mut files = BTreeMap::new();
  let runtime = Runtime {
    root: root.to_path_buf(),
  };
  fs::create_dir_all(root.join("bin")).unwrap();
  for tool in TOOLS {
    fs::write(runtime.bin(tool), tool).unwrap();
    let name = runtime
      .bin(tool)
      .strip_prefix(root)
      .unwrap()
      .to_string_lossy()
      .replace('\\', "/");
    files.insert(name, file_hash(&runtime.bin(tool)).unwrap());
  }
  fs::create_dir_all(root.join("share/extension")).unwrap();
  fs::create_dir_all(root.join("lib")).unwrap();
  for name in [
    "share/extension/pg_trgm.control",
    "share/extension/pg_trgm--1.6.sql",
    "lib/pg_trgm.so",
  ] {
    fs::write(root.join(name), "fixture").unwrap();
    files.insert(name.into(), file_hash(&root.join(name)).unwrap());
  }
  serde_json::json!({"version": VERSION, "target": target_slug(), "required": files.keys().collect::<Vec<_>>(), "files": files})
}

#[test]
fn manifest_checks_target_version_hash_and_required_files() {
  let root = Scratch::new();
  let mut manifest = fake_manifest(&root.0);
  let cache = root.0.join("cache");
  let path = root.0.join("manifest.json");
  fs::write(&path, manifest.to_string()).unwrap();
  assert!(Runtime::validate(&root.0, &cache).is_ok());
  manifest["version"] = "17.6".into();
  fs::write(&path, manifest.to_string()).unwrap();
  assert!(Runtime::validate(&root.0, &cache).is_err());
  manifest["version"] = VERSION.into();
  manifest["target"] = "wrong-target".into();
  fs::write(&path, manifest.to_string()).unwrap();
  assert!(Runtime::validate(&root.0, &cache).is_err());
  manifest["target"] = target_slug().into();
  fs::write(&path, manifest.to_string()).unwrap();
  fs::remove_file(&cache).unwrap();
  fs::write(
    Runtime {
      root: root.0.clone(),
    }
    .bin("psql"),
    "tampered",
  )
  .unwrap();
  assert!(Runtime::validate(&root.0, &cache)
    .err()
    .unwrap()
    .contains("checksum"));
  fs::remove_file(root.0.join("share/extension/pg_trgm.control")).unwrap();
  assert!(Runtime::validate(&root.0, &cache)
    .err()
    .unwrap()
    .contains("Missing"));
}

fn utc(value: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(value)
    .unwrap()
    .with_timezone(&Utc)
}

/// A completed dump named as the app names it, for the injected time `at`.
fn write_dump(dir: &Path, at: &str, suffix: &str) -> PathBuf {
  let path = dir.join(dump_name(utc(at), suffix));
  fs::write(&path, "successful dump").unwrap();
  path
}

const HOUR: Duration = Duration::from_secs(3600);

#[test]
fn dump_names_carry_their_own_utc_time() {
  let name = dump_name(utc("2026-10-11T02:00:05Z"), "1a2b3c4d");
  assert_eq!(name, "komika-20261011T020005Z-1a2b3c4d.dump");
  assert_eq!(recorded_time(&name), Some(utc("2026-10-11T02:00:05Z")));
  // The gate's older fixtures carry no random part.
  assert_eq!(
    recorded_time("komika-20000101T000003Z.dump"),
    Some(utc("2000-01-01T00:00:03Z"))
  );
  for other in [
    "komika-0.dump",
    "komika-20261011T020005Zx.dump",
    "komika-20261311T020005Z-a.dump",
    "other-20261011T020005Z-a.dump",
    "komika-20261011T020005Z-a.dump.tmp",
  ] {
    assert_eq!(recorded_time(other), None, "{other}");
  }
}

#[test]
fn dump_is_due_without_one_or_when_the_newest_is_twenty_hours_old() {
  let root = Scratch::new();
  let log = LogBuf::new();
  let mut schedule = DumpSchedule::new(root.0.join("backups"));
  let now = utc("2026-10-11T10:00:00Z");
  assert_eq!(schedule.poll(now, Duration::ZERO, &log), DumpDecision::Due);
  fs::create_dir(root.0.join("backups")).unwrap();
  assert_eq!(schedule.poll(now, Duration::ZERO, &log), DumpDecision::Due);
  // An unfinished dump and an unrelated file are not dumps.
  fs::write(
    root
      .0
      .join("backups/komika-20261011T095900Z-aaaaaaaa.dump.tmp"),
    "partial",
  )
  .unwrap();
  fs::write(root.0.join("backups/notes.dump"), "unrelated").unwrap();
  assert_eq!(schedule.poll(now, Duration::ZERO, &log), DumpDecision::Due);
  write_dump(&root.0.join("backups"), "2026-10-10T14:00:01Z", "aaaaaaaa");
  assert_eq!(
    schedule.poll(now, Duration::ZERO, &log),
    DumpDecision::Recent,
    "19 h 59 min 59 s old"
  );
  assert_eq!(
    schedule.poll(utc("2026-10-11T10:00:01Z"), Duration::ZERO, &log),
    DumpDecision::Due,
    "20 hours old"
  );
  assert!(log.text().is_empty(), "nothing to warn about");
}

/// Restart loop: every launch looks at the folder afresh. One good dump keeps all of
/// them quiet for 20 hours, and the dump lock keeps two writers apart.
#[test]
fn restart_loop_makes_one_dump_per_night() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let log = LogBuf::new();
  let mut made = 0;
  let mut now = utc("2026-10-11T22:00:00Z");
  // 200 launches, 6 minutes apart: 20 hours.
  for launch in 0..200 {
    let mut schedule = DumpSchedule::new(backups.clone());
    if schedule.poll(now, Duration::ZERO, &log) == DumpDecision::Due {
      let lock = DumpLock::acquire(&backups).unwrap().expect("no other dump");
      assert!(
        DumpLock::acquire(&backups).unwrap().is_none(),
        "a second dump cannot start while one is written"
      );
      fs::write(
        backups.join(dump_name(now, &format!("{launch:08}"))),
        "successful dump",
      )
      .unwrap();
      drop(lock);
      made += 1;
    }
    now += chrono::Duration::minutes(6);
  }
  assert_eq!(made, 1, "one dump in 20 hours of restarts");
  let mut schedule = DumpSchedule::new(backups.clone());
  assert_eq!(
    schedule.poll(now, Duration::ZERO, &log),
    DumpDecision::Due,
    "the next night's dump"
  );
  assert!(DumpLock::acquire(&backups).unwrap().is_some(), "released");
}

/// Forwards jump: a clock a year ahead makes every dump look old, so one dump is made
/// and dated a year ahead. Nothing is suppressed; the older dumps stay.
#[test]
fn clock_jumping_forwards_makes_one_dump_and_keeps_the_older_ones() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let log = LogBuf::new();
  let yesterday = write_dump(&backups, "2026-10-10T22:00:00Z", "aaaaaaaa");
  let mut schedule = DumpSchedule::new(backups.clone());
  assert_eq!(
    schedule.poll(utc("2026-10-11T08:00:00Z"), Duration::ZERO, &log),
    DumpDecision::Recent
  );
  let jumped = utc("2027-10-11T08:00:30Z");
  assert_eq!(
    schedule.poll(jumped, Duration::from_secs(30), &log),
    DumpDecision::Due
  );
  let ahead = write_dump(&backups, "2027-10-11T08:00:30Z", "bbbbbbbb");
  assert!(retain_dumps(&backups, &ahead, jumped).unwrap().is_empty());
  assert_eq!(
    schedule.poll(jumped, Duration::from_secs(30), &log),
    DumpDecision::Recent
  );
  assert!(yesterday.exists() && ahead.exists());
  assert!(!backups.join("dump-schedule.json").exists());
}

/// Backwards correction (N4): the newest dump is dated a year ahead of the corrected
/// clock. Dumps must not stop for a year, must not run at every look, and the
/// future-dated dump must never be removed for its date.
#[test]
fn clock_corrected_backwards_dumps_again_after_one_interval_of_running_time() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let log = LogBuf::new();
  let ahead = write_dump(&backups, "2027-10-11T08:00:30Z", "bbbbbbbb");
  let old = write_dump(&backups, "2026-10-01T02:00:00Z", "aaaaaaaa");
  let mut now = utc("2026-10-11T09:00:00Z");
  let mut schedule = DumpSchedule::new(backups.clone());
  assert_eq!(
    schedule.poll(now, Duration::ZERO, &log),
    DumpDecision::FutureDated {
      name: "komika-20271011T080030Z-bbbbbbbb.dump".into(),
      remaining: DUMP_INTERVAL
    },
    "not at every look"
  );
  assert!(log.text().contains("dated in the future"), "{}", log.text());
  // Within the tolerance a slightly-ahead dump is an ordinary recent dump.
  {
    let near = Scratch::new();
    write_dump(&near.0, "2026-10-11T09:04:59Z", "cccccccc");
    assert_eq!(
      DumpSchedule::new(near.0.clone()).poll(now, Duration::ZERO, &log),
      DumpDecision::Recent
    );
  }
  // The app runs 7 hours, is closed, and is opened again: the running time is kept.
  for _ in 0..7 {
    now += chrono::Duration::hours(1);
    assert!(matches!(
      schedule.poll(now, HOUR, &log),
      DumpDecision::FutureDated { .. }
    ));
  }
  let mut schedule = DumpSchedule::new(backups.clone());
  now += chrono::Duration::days(3); // closed for three days: no running time
  assert_eq!(
    schedule.poll(now, Duration::ZERO, &log),
    DumpDecision::FutureDated {
      name: "komika-20271011T080030Z-bbbbbbbb.dump".into(),
      remaining: 13 * HOUR
    }
  );
  // The clock jumps about meanwhile; only running time counts.
  now -= chrono::Duration::days(40);
  for _ in 0..12 {
    assert!(matches!(
      schedule.poll(now, HOUR, &log),
      DumpDecision::FutureDated { .. }
    ));
  }
  assert_eq!(
    schedule.poll(now, HOUR, &log),
    DumpDecision::Due,
    "20 hours of running time: the future-dated dump no longer counts as recent"
  );
  let fresh = backups.join(dump_name(now, "dddddddd"));
  fs::write(&fresh, "successful dump").unwrap();
  assert!(retain_dumps(&backups, &fresh, now).unwrap().is_empty());
  assert!(
    ahead.exists(),
    "never removed for being dated in the future"
  );
  assert!(old.exists() && fresh.exists());
  // From here the ordinary rule runs on the dumps that are not in the future, also
  // after a restart: quiet for 20 hours, then due.
  let mut schedule = DumpSchedule::new(backups.clone());
  assert_eq!(
    schedule.poll(now, Duration::ZERO, &log),
    DumpDecision::Recent
  );
  now += chrono::Duration::hours(19);
  assert_eq!(schedule.poll(now, 19 * HOUR, &log), DumpDecision::Recent);
  now += chrono::Duration::hours(1);
  assert_eq!(schedule.poll(now, HOUR, &log), DumpDecision::Due);
  assert_eq!(
    log.text().matches("dated in the future").count(),
    3,
    "said once per launch that met it, not at every look"
  );
}

/// The checker's retention case: seven older dumps are dated in the future, a new dump
/// is written after the clock was corrected. The new dump sorts last by time and must
/// still be the one that is never removed.
#[test]
fn retention_never_removes_the_dump_it_just_wrote() {
  let root = Scratch::new();
  let mut ahead = Vec::new();
  for day in 1..=7 {
    ahead.push(write_dump(
      &root.0,
      &format!("2027-10-{day:02}T02:00:00Z"),
      "aaaaaaaa",
    ));
  }
  let fresh = write_dump(&root.0, "2026-10-11T09:00:00Z", "bbbbbbbb");
  let removed = retain_dumps(&root.0, &fresh, utc("2026-10-11T09:00:00Z")).unwrap();
  assert!(fresh.exists(), "the new dump is kept");
  assert_eq!(
    removed,
    vec![ahead[0].clone()],
    "the oldest of the others goes"
  );
  assert_eq!(dumps(&root.0).unwrap().len(), DUMP_KEEP);
}

#[test]
fn retention_with_nine_existing_dumps_keeps_the_seven_newest() {
  let root = Scratch::new();
  let mut older = Vec::new();
  for day in 1..=9 {
    older.push(write_dump(
      &root.0,
      &format!("2026-10-{day:02}T02:00:00Z"),
      "aaaaaaaa",
    ));
  }
  fs::write(
    root.0.join("komika-20261010T020000Z-cccccccc.dump.tmp"),
    "failed dump",
  )
  .unwrap();
  fs::write(root.0.join("other.dump"), "unrelated").unwrap();
  let now = utc("2026-10-10T02:00:05Z");
  // A new dump that is missing or empty removes nothing.
  let missing = root
    .0
    .join(dump_name(utc("2026-10-10T02:00:00Z"), "bbbbbbbb"));
  assert!(retain_dumps(&root.0, &missing, now).is_err());
  fs::write(&missing, "").unwrap();
  assert!(retain_dumps(&root.0, &missing, now).is_err());
  assert_eq!(dumps(&root.0).unwrap().len(), 10, "nothing removed");
  fs::write(&missing, "successful dump").unwrap();
  let fresh = missing;
  let mut removed = retain_dumps(&root.0, &fresh, now).unwrap();
  removed.sort();
  assert_eq!(removed, older[..3].to_vec(), "the three oldest");
  let kept = dumps(&root.0).unwrap();
  assert_eq!(kept.len(), DUMP_KEEP);
  assert_eq!(kept[0], fresh, "newest first");
  assert!(root.0.join("other.dump").exists());
  assert!(root
    .0
    .join("komika-20261010T020000Z-cccccccc.dump.tmp")
    .exists());
  // A killed run's unfinished dumps go once the lock proves nobody writes them.
  remove_stale_temporaries(&root.0);
  assert!(!root
    .0
    .join("komika-20261010T020000Z-cccccccc.dump.tmp")
    .exists());
  assert_eq!(dumps(&root.0).unwrap().len(), DUMP_KEEP);
}

#[test]
fn fake_process_tables_require_exact_executable_and_data_argument() {
  let root = Scratch::new();
  let image = root.0.join("postgres");
  let foreign = root.0.join("other-postgres");
  fs::write(&image, "fake").unwrap();
  fs::write(&foreign, "fake").unwrap();
  let data = root.0.join("data");
  fs::create_dir(&data).unwrap();
  let pid = process::PidFile {
    pid: 222,
    data: data.clone(),
    started: None,
  };
  let identity = process::Identity {
    pid: 222,
    image: image.clone(),
    created: "first".into(),
    args: vec![
      image.display().to_string(),
      "-D".into(),
      data.display().to_string(),
    ],
  };
  assert!(process::owns(&identity, &image, &data, &pid));
  let mut other = identity.clone();
  other.image = foreign;
  assert!(!process::owns(&other, &image, &data, &pid));
  other = identity.clone();
  other.args[2] = root.0.display().to_string();
  assert!(!process::owns(&other, &image, &data, &pid));
  other = identity;
  other.pid = 223;
  assert!(!process::owns(&other, &image, &data, &pid));
}

/// Data folders stay private even when a session with umask 002 creates folders 0775.
#[cfg(unix)]
#[test]
fn data_folder_is_created_and_kept_private_and_a_link_or_foreign_folder_is_left_alone() {
  use std::os::unix::fs::{MetadataExt, PermissionsExt};
  let mode = |path: &Path| fs::symlink_metadata(path).unwrap().mode() & 0o7777;
  let root = Scratch::new();
  let me = fs::metadata(&root.0).unwrap().uid();

  // New, with missing parents: every folder it creates is private.
  let fresh = root.0.join("app data/server");
  assert_eq!(paths::private_dir_as(&fresh, me).unwrap(), None);
  assert_eq!(mode(&fresh), 0o700);
  assert_eq!(mode(fresh.parent().unwrap()), 0o700);

  // Existing and open to the group and to others (what umask 002 or a manual chmod
  // leaves): tightened at the next start, content untouched.
  let open = root.0.join("open");
  fs::create_dir(&open).unwrap();
  fs::write(open.join("kept.txt"), "kept").unwrap();
  for loose in [0o775, 0o777, 0o750, 0o705] {
    fs::set_permissions(&open, fs::Permissions::from_mode(loose)).unwrap();
    assert_eq!(paths::private_dir_as(&open, me).unwrap(), None, "{loose:o}");
    assert_eq!(mode(&open), 0o700, "{loose:o}");
  }
  assert_eq!(fs::read_to_string(open.join("kept.txt")).unwrap(), "kept");
  // The public entry point acts for the user that runs it.
  fs::set_permissions(&open, fs::Permissions::from_mode(0o775)).unwrap();
  assert_eq!(private_dir(&open).unwrap(), None);
  assert_eq!(mode(&open), 0o700);

  // Another owner: nothing is changed, the owner of the machine is told.
  fs::set_permissions(&open, fs::Permissions::from_mode(0o775)).unwrap();
  let warning = paths::private_dir_as(&open, me + 1).unwrap().unwrap();
  assert!(warning.contains("belongs to another user"), "{warning}");
  assert!(warning.contains(&open.display().to_string()), "{warning}");
  assert_eq!(mode(&open), 0o775);

  // A link is never followed: the folder behind it keeps its mode, and the warning
  // comes only when that folder is writable by other users or has another owner.
  let link = root.0.join("link");
  std::os::unix::fs::symlink(&open, &link).unwrap();
  let warning = paths::private_dir_as(&link, me).unwrap().unwrap();
  assert!(warning.contains("is a link"), "{warning}");
  assert_eq!(mode(&open), 0o775);
  fs::set_permissions(&open, fs::Permissions::from_mode(0o700)).unwrap();
  assert_eq!(paths::private_dir_as(&link, me).unwrap(), None);
  assert!(fs::symlink_metadata(&link)
    .unwrap()
    .file_type()
    .is_symlink());

  // A plain file at the name is an error, not a silent success.
  let file = root.0.join("file");
  fs::write(&file, "").unwrap();
  assert!(paths::private_dir_as(&file, me).is_err());
}

// ---- postmaster.pid whose process is not ours (check D round 3: A1, B1, B5) -------------

/// A data folder, the bundled program and another `postgres`, as files on disk (the
/// decision compares resolved paths).
struct Install {
  _root: Scratch,
  program: PathBuf,
  elsewhere: PathBuf,
  data: PathBuf,
  other_data: PathBuf,
}
impl Install {
  fn new() -> Self {
    let root = Scratch::new();
    let program = root.0.join("runtime/bin/postgres");
    let elsewhere = root.0.join("homebrew/bin/postgres");
    for file in [&program, &elsewhere] {
      fs::create_dir_all(file.parent().unwrap()).unwrap();
      fs::write(file, "fake").unwrap();
    }
    let data = root.0.join("server/postgres/18/data");
    let other_data = root.0.join("other install/data");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(&other_data).unwrap();
    Self {
      _root: root,
      program,
      elsewhere,
      data,
      other_data,
    }
  }
  fn pid_file(&self) -> process::PidFile {
    process::PidFile {
      pid: 4242,
      data: self.data.clone(),
      started: None,
    }
  }
  fn postmaster(&self, image: &Path, data: &Path) -> process::Identity {
    process::Identity {
      pid: 4242,
      image: image.to_path_buf(),
      created: "1:2".into(),
      args: vec![
        image.display().to_string(),
        "-D".into(),
        data.display().to_string(),
        "-p".into(),
        "5555".into(),
      ],
    }
  }
  /// The decision for a PID held by `holder`, with `identity` as the answer of the
  /// detailed look and `place` as the answer about its working folder. `None`: that
  /// look must not happen at all.
  fn claim(
    &self,
    holder: process::Holder,
    identity: Option<Result<Option<process::Identity>, String>>,
    place: Option<process::Place>,
  ) -> process::Claim {
    self.claim_with(ONE_NAME, holder, identity, place)
  }
  /// [`Install::claim`] with what is known about the system and the lock file.
  fn claim_with(
    &self,
    facts: process::LockFacts,
    holder: process::Holder,
    identity: Option<Result<Option<process::Identity>, String>>,
    place: Option<process::Place>,
  ) -> process::Claim {
    process::claim(
      &self.pid_file(),
      &self.program,
      &self.data,
      facts,
      holder,
      |pid| {
        assert_eq!(pid, 4242);
        identity.expect("the arguments of this process must not be read")
      },
      |pid| {
        assert_eq!(pid, 4242);
        place.expect("the working folder of this process must not be read")
      },
    )
  }
}

/// A system that reads working folders (Unix), and a data folder in which no file that
/// was looked at is shared with another folder.
const ONE_NAME: process::LockFacts = process::LockFacts {
  folders_read: true,
  shared: None,
};

fn foreign(claim: process::Claim) -> process::Foreign {
  match claim {
    process::Claim::Foreign(foreign) => foreign,
    other => panic!("expected a foreign process, got {other:?}"),
  }
}

fn adopted(claim: process::Claim) -> process::Adopted {
  match claim {
    process::Claim::Here(found) => found,
    other => panic!("expected this folder's database, got {other:?}"),
  }
}

fn elsewhere() -> process::Place {
  process::Place::Elsewhere(Some("/another/data".into()))
}

fn unknown() -> process::Place {
  process::Place::Unknown("Operation not permitted".into())
}

#[test]
fn lock_file_of_no_process_is_left_to_postgresql() {
  let install = Install::new();
  assert_eq!(
    install.claim(process::Holder::Free, None, None),
    process::Claim::Stale
  );
  // It ended between the cheap look and the detailed one.
  assert_eq!(
    install.claim(
      process::Holder::Program(install.program.clone()),
      Some(Ok(None)),
      None
    ),
    process::Claim::Stale
  );
}

#[test]
fn lock_file_of_this_installs_postgresql_on_this_data_folder_is_ours() {
  let install = Install::new();
  let postmaster = install.postmaster(&install.program, &install.data);
  let claim = |facts, place| {
    install.claim_with(
      facts,
      process::Holder::Program(install.program.clone()),
      Some(Ok(Some(postmaster.clone()))),
      place,
    )
  };
  assert_eq!(
    claim(ONE_NAME, Some(process::Place::Here)),
    process::Claim::Ours(postmaster.clone())
  );
  // M2: this install's program, this data folder in its `-D` argument and in the lock
  // file, and it runs in ANOTHER folder. The app data folder was renamed while the
  // database ran, and a copy of it (lock file included) was put at the old path. A stop
  // sent through this folder's lock file would reach the database of the renamed folder,
  // which would write its control file here. Not ours; and never disregarded, because it
  // still writes its control file by this path.
  let renamed_away = foreign(claim(ONE_NAME, Some(elsewhere())));
  assert!(!renamed_away.disregard, "{renamed_away:?}");
  assert!(
    renamed_away.what.contains(
      "started on this data folder's path and now runs in another folder (/another/data)"
    ),
    "{}",
    renamed_away.what
  );
  // Its working folder cannot be read: not proven to be this folder's, so not ours.
  let unread = foreign(claim(ONE_NAME, Some(unknown())));
  assert!(!unread.disregard, "{unread:?}");
  // Windows reads no working folder: there the texts are all the proof there is, and
  // the working folder is not asked for.
  let windows = process::LockFacts {
    folders_read: false,
    shared: Some("the names of its files are not read on this system".into()),
  };
  assert_eq!(claim(windows, None), process::Claim::Ours(postmaster));
}

/// M3, A1 and F1: a copy of a data folder made with hard links shares its files with the
/// folder it was copied from. The live database of that folder runs "in another folder"
/// and is still a database on this folder's files. PostgreSQL's own refusal of the lock
/// file is all that keeps a second server off them, so PostgreSQL is never told to
/// disregard a PostgreSQL when a file of this folder that was looked at (the lock file,
/// everything directly in `global/` and `pg_wal/`) has more than one name.
#[test]
fn a_postgresql_is_never_disregarded_when_the_data_folder_shares_files() {
  let install = Install::new();
  let other = install.postmaster(&install.elsewhere, &install.other_data);
  let seen = |facts| {
    foreign(install.claim_with(
      facts,
      process::Holder::Program(install.elsewhere.clone()),
      Some(Ok(Some(other.clone()))),
      Some(elsewhere()),
    ))
  };
  let shared = |reason: &str| process::LockFacts {
    folders_read: true,
    shared: Some(reason.into()),
  };
  assert!(seen(ONE_NAME).disregard, "control: nothing shared");
  let refused = seen(shared(
    "the file global/1213 of this data folder has 2 names",
  ));
  assert!(!refused.disregard, "{refused:?}");
  assert_eq!(
    refused.what,
    "a PostgreSQL of another data folder (/another/data); the file global/1213 of this \
     data folder has 2 names"
  );
  // Another program is not a database on any folder, whatever the files are.
  assert!(
    foreign(install.claim_with(
      shared("anything"),
      process::Holder::Program("/bin/sleep".into()),
      None,
      None
    ))
    .disregard
  );

  // What is read from real files: a made-up cluster, and a copy of it.
  #[cfg(unix)]
  {
    let files = [
      "postmaster.pid",
      "global/1213",
      "global/pg_control",
      "global/pg_filenode.map",
      "pg_wal/000000010000000000000001",
      "base/5/1259",
    ];
    let original = install.other_data.clone();
    let copy = install.data.clone();
    for folder in [&original, &copy] {
      for inner in ["global", "pg_wal/archive_status", "base/5"] {
        fs::create_dir_all(folder.join(inner)).unwrap();
      }
    }
    for file in files {
      fs::write(original.join(file), format!("content of {file}")).unwrap();
    }
    let lock = copy.join("postmaster.pid");
    let read = || process::LockFacts::read(&lock);
    let reason = || {
      read()
        .shared
        .expect("the copy is not known to be on its own")
    };
    // An ordinary file in place of the hard link: the same content, one name.
    let own = |file: &str| {
      let _ = fs::remove_file(copy.join(file));
      fs::write(copy.join(file), format!("content of {file}")).unwrap();
    };
    assert_eq!(
      process::LockFacts::read(&original.join("postmaster.pid")),
      ONE_NAME
    );
    // `cp -al`: every file is the same file.
    for file in files {
      fs::hard_link(original.join(file), copy.join(file)).unwrap();
    }
    assert!(
      reason().starts_with("the file postmaster.pid of this data folder has 2 names (hard links)"),
      "{}",
      reason()
    );
    assert!(
      reason().contains("can be the database of both"),
      "{}",
      reason()
    );
    assert!(!seen(read()).disregard);
    // Round 6, A1: the lock file of the copy is a file of its own. The rest is shared.
    own("postmaster.pid");
    assert!(
      reason().starts_with("the file global/1213 of this data folder has 2 names (hard links)"),
      "{}",
      reason()
    );
    // Round 7, F1 (the checker's variant): the control file is a file of its own too.
    // Round 6 looked at these two files only and told PostgreSQL to disregard the live
    // database of the original. The other files of `global/` and the log still tell.
    own("global/pg_control");
    assert!(reason().contains("global/1213"), "{}", reason());
    assert!(!seen(read()).disregard);
    own("global/1213");
    assert!(reason().contains("global/pg_filenode.map"), "{}", reason());
    own("global/pg_filenode.map");
    assert!(
      reason().starts_with(
        "the file pg_wal/000000010000000000000001 of this data folder has 2 names (hard links)"
      ),
      "{}",
      reason()
    );
    assert!(!seen(read()).disregard);
    // The recorded limit: only a relation file under `base/` is still shared. That is
    // not seen, and the process of the other folder is disregarded.
    own("pg_wal/000000010000000000000001");
    {
      use std::os::unix::fs::MetadataExt;
      assert_eq!(fs::metadata(copy.join("base/5/1259")).unwrap().nlink(), 2);
    }
    assert_eq!(read(), ONE_NAME);
    assert!(seen(read()).disregard);
    // A file of `global/` that is a link to another file counts as shared.
    fs::remove_file(copy.join("global/1213")).unwrap();
    std::os::unix::fs::symlink(original.join("global/1213"), copy.join("global/1213")).unwrap();
    assert!(
      reason().starts_with("the file global/1213 of this data folder is a link to another file"),
      "{}",
      reason()
    );
    own("global/1213");
    assert_eq!(read(), ONE_NAME);
    // Round 8: the log folder outside the data folder, the way PostgreSQL supports it
    // (`pg_wal` is a link), and a copy of the data folder that kept the link. The lock
    // file and every file of `global/` are ordinary copies, and each log file has one
    // name: both data folders reach the same log files through the link. Round 7
    // listed the folder through the link, found one name everywhere, and told
    // PostgreSQL to disregard the live database of the original.
    {
      use std::os::unix::fs::MetadataExt;
      let outside = install.other_data.with_file_name("log on another disk");
      fs::create_dir(&outside).unwrap();
      let segment = "000000010000000000000001";
      fs::write(outside.join(segment), "the one log both would write").unwrap();
      for folder in [&original, &copy] {
        fs::remove_dir_all(folder.join("pg_wal")).unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("pg_wal")).unwrap();
      }
      for file in ["postmaster.pid", "global/1213", "global/pg_control"] {
        assert_eq!(fs::metadata(copy.join(file)).unwrap().nlink(), 1, "{file}");
      }
      assert_eq!(
        fs::metadata(copy.join("pg_wal").join(segment))
          .unwrap()
          .nlink(),
        1,
        "one name, reached from both folders"
      );
      assert_eq!(
        reason(),
        "the folder pg_wal of this data folder is a link to another folder, so its files \
         can be the files of another data folder as well, and that process can be the \
         database of both"
      );
      let refused = seen(read());
      assert!(!refused.disregard, "{refused:?}");
      assert!(
        refused
          .what
          .ends_with("and that process can be the database of both"),
        "{refused:?}"
      );
      // The same for `global/` as a link.
      fs::remove_file(copy.join("pg_wal")).unwrap();
      fs::create_dir_all(copy.join("pg_wal/archive_status")).unwrap();
      own("pg_wal/000000010000000000000001");
      assert_eq!(read(), ONE_NAME);
      fs::rename(copy.join("global"), outside.join("global")).unwrap();
      std::os::unix::fs::symlink(outside.join("global"), copy.join("global")).unwrap();
      assert!(
        reason().starts_with("the folder global of this data folder is a link to another folder"),
        "{}",
        reason()
      );
      assert!(!seen(read()).disregard);
      fs::remove_file(copy.join("global")).unwrap();
      fs::rename(outside.join("global"), copy.join("global")).unwrap();
      assert_eq!(read(), ONE_NAME);
    }
    // Nothing to count: no proof, so no disregard.
    fs::remove_dir_all(copy.join("pg_wal")).unwrap();
    assert!(
      reason().starts_with("the folder pg_wal of this data folder could not be read"),
      "{}",
      reason()
    );
    fs::remove_dir_all(copy.join("global")).unwrap();
    assert!(
      reason().starts_with("the folder global of this data folder could not be read"),
      "{}",
      reason()
    );
    fs::remove_file(&lock).unwrap();
    assert!(
      reason().starts_with(
        "how many names the file postmaster.pid of this data folder has could not be read"
      ),
      "{}",
      reason()
    );
    assert!(!seen(read()).disregard);
  }
}

/// The checker's two cases: a live process of this user that is not PostgreSQL, and a
/// process of another user. Both used to block the start for good.
#[test]
fn lock_file_of_a_reused_pid_is_treated_as_left_over_and_the_process_is_named() {
  let install = Install::new();
  // Another program of this user: neither its arguments (on Windows that would start a
  // helper program) nor its working folder are read, and PostgreSQL may be told to
  // disregard the PID.
  let sleep = foreign(install.claim(process::Holder::Program("/bin/sleep".into()), None, None));
  assert_eq!(sleep.pid, 4242);
  assert_eq!(sleep.image.as_deref(), Some(Path::new("/bin/sleep")));
  assert!(sleep.disregard);
  assert_eq!(sleep.named(), "process 4242 (/bin/sleep)");
  let line = stale_pid_line(&install.data.join("postmaster.pid"), &sleep);
  assert!(
    line.contains("process 4242 (/bin/sleep)")
      && line.contains("was not stopped")
      && line.contains("was not removed")
      && line.contains("postmaster.pid"),
    "{line}"
  );
  assert_eq!(line.lines().count(), 1, "one line");

  // A process that cannot be inspected (root's; on Linux also a live process whose
  // program file link cannot be read): no image is known, nothing is said to PostgreSQL.
  let system = foreign(install.claim(
    process::Holder::Unreadable("Cannot inspect process 4242: Operation not permitted".into()),
    None,
    None,
  ));
  assert_eq!(system.image, None);
  assert!(!system.disregard);
  assert_eq!(system.named(), "process 4242");
  assert!(
    system.what.contains("Operation not permitted"),
    "{}",
    system.what
  );

  // A process that has ended and waits to be collected runs nothing.
  let ended = foreign(install.claim(process::Holder::Ended, None, None));
  assert!(ended.disregard && ended.image.is_none());
}

/// F1: whether a live PostgreSQL is "of another data folder" is decided by the folder it
/// runs in (device and inode), never by the text of its `-D` argument or of the lock
/// file. PostgreSQL is told to disregard its PID only when that folder was read and is
/// not this data folder.
#[test]
fn a_live_postgresql_is_placed_by_its_working_folder_not_by_its_arguments() {
  let install = Install::new();
  let holder = |image: &Path| process::Holder::Program(image.to_path_buf());

  // The checker's renamed-folder case. The app data folder was renamed while the
  // database ran, and an empty folder was made at the old path. The process still says
  // `-D <old path>`, and the old path exists and is not this data folder: by the text it
  // is "a PostgreSQL of another data folder". By its working folder it is this folder's
  // database. It is never disregarded; it is adopted.
  let renamed = install.postmaster(&install.program, &install.other_data);
  let found = adopted(install.claim(
    holder(&install.program),
    Some(Ok(Some(renamed.clone()))),
    Some(process::Place::Here),
  ));
  assert_eq!(found.identity, renamed);
  assert!(found.own_program);
  // The same process when its working folder cannot be read: nothing is proven, so
  // nothing is said to PostgreSQL, which decides alone.
  let unread = foreign(install.claim(
    holder(&install.program),
    Some(Ok(Some(renamed.clone()))),
    Some(unknown()),
  ));
  assert!(!unread.disregard, "{unread:?}");
  assert!(unread.what.contains("could not be read"), "{}", unread.what);
  // And when it was read and is another folder: provably not here.
  let twin = foreign(install.claim(
    holder(&install.program),
    Some(Ok(Some(renamed))),
    Some(elsewhere()),
  ));
  assert!(twin.disregard, "{twin:?}");
  assert!(
    twin.what.contains("another data folder (/another/data)"),
    "{}",
    twin.what
  );

  // Another PostgreSQL (another path) of another data folder.
  let other = install.postmaster(&install.elsewhere, &install.other_data);
  assert!(
    foreign(install.claim(
      holder(&install.elsewhere),
      Some(Ok(Some(other.clone()))),
      Some(elsewhere())
    ))
    .disregard
  );
  assert!(
    !foreign(install.claim(
      holder(&install.elsewhere),
      Some(Ok(Some(other))),
      Some(unknown())
    ))
    .disregard
  );

  // F2: a PostgreSQL from another place that runs IN this data folder (the app was moved
  // or replaced while its database was left running): this folder's database, adopted,
  // marked as not this install's program.
  let moved = install.postmaster(&install.elsewhere, &install.data);
  let found = adopted(install.claim(
    holder(&install.elsewhere),
    Some(Ok(Some(moved.clone()))),
    Some(process::Place::Here),
  ));
  assert!(!found.own_program);
  // Its arguments name this data folder, but it does not run in it (the folder it runs
  // in was renamed away and this is a new folder at the old path): not this folder's
  // database. It still writes its control file by this path, so it is not disregarded
  // either: PostgreSQL refuses to start beside it (round 5, M1 and M2).
  let away = foreign(install.claim(
    holder(&install.elsewhere),
    Some(Ok(Some(moved.clone()))),
    Some(elsewhere()),
  ));
  assert!(!away.disregard, "{away:?}");
  assert!(
    away.what.contains("started on this data folder's path"),
    "{}",
    away.what
  );
  assert!(
    !foreign(install.claim(
      holder(&install.elsewhere),
      Some(Ok(Some(moved))),
      Some(unknown())
    ))
    .disregard
  );

  // A helper process of a database shows a title, no `-D`. In this data folder it is
  // part of this folder's database: never disregarded, and never stopped in place of the
  // main process. In another folder it is provably not here.
  let mut worker = install.postmaster(&install.elsewhere, &install.other_data);
  worker.args = vec!["postgres: checkpointer ".into()];
  let here = foreign(install.claim(
    holder(&install.elsewhere),
    Some(Ok(Some(worker.clone()))),
    Some(process::Place::Here),
  ));
  assert!(!here.disregard, "{here:?}");
  assert!(
    here.what.contains("runs in this data folder"),
    "{}",
    here.what
  );
  assert!(
    foreign(install.claim(
      holder(&install.elsewhere),
      Some(Ok(Some(worker))),
      Some(elsewhere())
    ))
    .disregard
  );

  // Its arguments cannot be read (Windows: the query failed or timed out). In this data
  // folder: this folder's, but it cannot be identified for a stop. Never disregarded.
  let blind = |place| {
    foreign(install.claim(
      holder(&install.program),
      Some(Err("Process argument query timed out".into())),
      Some(place),
    ))
  };
  let here = blind(process::Place::Here);
  assert!(!here.disregard);
  assert_eq!(here.image.as_deref(), Some(install.program.as_path()));
  assert!(
    here.what.contains("runs in this data folder"),
    "{}",
    here.what
  );
  assert!(!blind(unknown()).disregard);
  assert!(blind(elsewhere()).disregard);

  // Names that are PostgreSQL's on some system are never taken for "another program".
  for name in [
    "postgres.exe",
    "postgres (deleted)",
    "POSTGRES.EXE",
    "postmaster",
  ] {
    let image = install.elsewhere.with_file_name(name);
    let seen = foreign(install.claim(
      holder(&image),
      Some(Err("not read in this test".into())),
      Some(unknown()),
    ));
    assert!(!seen.disregard, "{name}");
  }
}

/// F2: the program file of the live process is gone (macOS reports that; the app was
/// deleted while its database ran). The name it was started under and its working
/// folder decide.
#[test]
fn a_live_process_whose_program_file_is_gone_is_placed_by_its_working_folder() {
  let install = Install::new();
  let gone = || process::Holder::Gone("the program file of process 4242 no longer exists".into());
  let old_place = Path::new("/Applications/Old Komiq.app/Contents/Resources/postgres/bin/postgres");
  let postmaster = install.postmaster(old_place, &install.data);
  let found = adopted(install.claim(
    gone(),
    Some(Ok(Some(postmaster.clone()))),
    Some(process::Place::Here),
  ));
  assert!(!found.own_program);
  assert_eq!(found.identity.image, old_place);
  // Not in this folder, started on this folder's path: not this folder's database, and
  // not disregarded (it writes its control file by this path).
  let other = foreign(install.claim(
    gone(),
    Some(Ok(Some(postmaster.clone()))),
    Some(elsewhere()),
  ));
  assert!(!other.disregard);
  assert_eq!(other.image.as_deref(), Some(old_place));
  // Started on another path and not in this folder: provably nothing to do with it.
  let unrelated = install.postmaster(old_place, &install.other_data);
  assert!(foreign(install.claim(gone(), Some(Ok(Some(unrelated))), Some(elsewhere()))).disregard);
  assert!(!foreign(install.claim(gone(), Some(Ok(Some(postmaster))), Some(unknown()))).disregard);
  // Started under a name that is not PostgreSQL's, or under no readable name: what it
  // is cannot be told. Nothing is said to PostgreSQL, and its working folder is not read.
  let sleep = install.postmaster(Path::new("/tmp/gone/sleep"), &install.data);
  let seen = foreign(install.claim(gone(), Some(Ok(Some(sleep))), None));
  assert!(!seen.disregard);
  assert!(seen.what.contains("no longer exists"), "{}", seen.what);
  let seen = foreign(install.claim(gone(), Some(Err("no arguments".into())), None));
  assert!(!seen.disregard && seen.image.is_none());
  // It ended meanwhile.
  assert_eq!(
    install.claim(gone(), Some(Ok(None)), None),
    process::Claim::Stale
  );
}

/// The variable is passed for a proven PID only, and never on Windows.
#[test]
fn postgresql_is_told_to_disregard_only_a_proven_pid_and_never_on_windows() {
  let note = |disregard| {
    LockNote::Foreign(process::Foreign {
      pid: 4242,
      image: Some("/bin/sleep".into()),
      what: "another program that was given this process number".into(),
      disregard,
    })
  };
  assert_eq!(grandparent_pid(Some(&note(true)), false), Some(4242));
  assert_eq!(grandparent_pid(Some(&note(false)), false), None);
  assert_eq!(grandparent_pid(Some(&note(true)), true), None, "Windows");
  assert_eq!(grandparent_pid(Some(&note(false)), true), None);
  assert_eq!(grandparent_pid(None, false), None);
  let garbled = LockNote::Unreadable("it is empty".into());
  assert_eq!(grandparent_pid(Some(&garbled), false), None);
  assert_eq!(grandparent_pid(Some(&garbled), true), None);
}

/// The two readings, each branch: the same folder by device and inode is "here"; a read
/// that failed on either side proves nothing.
#[test]
fn a_working_folder_is_compared_by_device_and_inode() {
  use process::{placed, FolderId, Place};
  let id = |device, inode| FolderId { device, inode };
  let at = |device, inode| {
    Ok(Some((
      id(device, inode),
      Some(PathBuf::from("/now/called")),
    )))
  };
  assert_eq!(placed(Ok(id(7, 42)), at(7, 42)), Place::Here);
  assert_eq!(
    placed(Ok(id(7, 42)), at(7, 43)),
    Place::Elsewhere(Some("/now/called".into())),
    "another inode"
  );
  assert_eq!(
    placed(Ok(id(7, 42)), at(8, 42)),
    Place::Elsewhere(Some("/now/called".into())),
    "the same inode number on another device"
  );
  assert_eq!(
    placed(Ok(id(7, 42)), Ok(Some((id(9, 9), None)))),
    Place::Elsewhere(None)
  );
  for unproven in [
    placed(Ok(id(7, 42)), Err("Operation not permitted".into())),
    placed(Ok(id(7, 42)), Ok(None)),
    placed(Err("/data: No such file or directory".into()), at(7, 42)),
  ] {
    assert!(matches!(unproven, Place::Unknown(_)), "{unproven:?}");
  }
}

/// The real thing on this machine: a live process of this user that is not PostgreSQL,
/// then the same PID after it ended but before it was collected, then after.
#[cfg(unix)]
#[test]
fn a_live_unrelated_process_is_seen_as_another_program() {
  let install = Install::new();
  let mut sleeper = std::process::Command::new("sleep")
    .arg("300")
    .spawn()
    .unwrap();
  let pid = sleeper.id();
  // Right after a spawn Linux may still show this test program for a moment.
  let own = std::env::current_exe().ok();
  let deadline = Instant::now() + Duration::from_secs(5);
  let seen = loop {
    let seen = process::holder(pid);
    let settled = !matches!(&seen, process::Holder::Program(image) if Some(image) == own.as_ref());
    if settled || Instant::now() >= deadline {
      break seen;
    }
    std::thread::sleep(Duration::from_millis(1));
  };
  let process::Holder::Program(image) = &seen else {
    panic!("{seen:?}")
  };
  // On some systems `sleep` is one program with many names (`coreutils`, `busybox`).
  assert!(
    !image
      .file_name()
      .unwrap()
      .to_string_lossy()
      .starts_with("postgres"),
    "{image:?}"
  );
  let file = process::PidFile {
    pid,
    data: install.data.clone(),
    started: None,
  };
  let claimed = process::claim(
    &file,
    &install.program,
    &install.data,
    ONE_NAME,
    seen.clone(),
    |_| panic!("the arguments of another program are not read"),
    |_| panic!("the working folder of another program is not read"),
  );
  let found = foreign(claimed);
  assert!(found.disregard);
  assert_eq!(found.pid, pid);
  // Ended, not yet collected by this test (its parent): the PID is still taken.
  sleeper.kill().unwrap();
  let deadline = Instant::now() + Duration::from_secs(5);
  // Between running and ended a process is, for a moment, neither (Linux: its program
  // is already let go and its state is not yet `Z`): unknown, which is never "ended".
  while process::holder(pid) != process::Holder::Ended && Instant::now() < deadline {
    std::thread::sleep(Duration::from_millis(20));
  }
  assert_eq!(process::holder(pid), process::Holder::Ended);
  sleeper.wait().unwrap();
  assert_eq!(process::holder(pid), process::Holder::Free);
  // Process 1 belongs to the system: either its program file can be read (and is not
  // PostgreSQL) or it cannot be inspected. Neither is "no such process".
  assert_ne!(process::holder(1), process::Holder::Free);
  assert_ne!(process::holder(1), process::Holder::Ended);
}

/// The real thing: the working folder of a live process is compared with a data folder
/// by the folder's identity. A rename of the folder changes nothing; another folder made
/// at the old path is another folder.
#[cfg(unix)]
#[test]
fn the_working_folder_of_a_live_process_follows_a_renamed_folder() {
  let root = Scratch::new();
  let a = root.0.join("app data A é/server/postgres/18/data");
  fs::create_dir_all(&a).unwrap();
  let mut child = std::process::Command::new("sleep")
    .arg("300")
    .current_dir(&a)
    .spawn()
    .unwrap();
  let pid = child.id();
  assert_eq!(process::place(pid, &a), process::Place::Here);
  assert!(matches!(
    process::place(pid, &root.0),
    process::Place::Elsewhere(_)
  ));
  // A folder that does not exist proves nothing.
  assert!(matches!(
    process::place(pid, &root.0.join("missing")),
    process::Place::Unknown(_)
  ));
  // Rename the app data folder and make an empty folder at the old path.
  let b = root.0.join("app data B é");
  fs::rename(root.0.join("app data A é"), &b).unwrap();
  fs::create_dir_all(&a).unwrap();
  let b_data = b.join("server/postgres/18/data");
  assert_eq!(
    process::place(pid, &b_data),
    process::Place::Here,
    "the process runs in the renamed folder"
  );
  let old_path = process::place(pid, &a);
  assert!(
    matches!(old_path, process::Place::Elsewhere(_)),
    "the new folder at the old path is another folder: {old_path:?}"
  );
  // A link to the data folder is the data folder.
  let link = root.0.join("link");
  std::os::unix::fs::symlink(&b_data, &link).unwrap();
  assert_eq!(process::place(pid, &link), process::Place::Here);
  // The process itself, from here: this test's own working folder is not that folder.
  assert!(matches!(
    process::place(std::process::id(), &b_data),
    process::Place::Elsewhere(_)
  ));
  child.kill().unwrap();
  child.wait().unwrap();
  // No such process: nothing is proven.
  assert!(matches!(
    process::place(pid, &b_data),
    process::Place::Unknown(_)
  ));
}

const IDLE_HELPER: &str = "KOMIQ_POSTGRES_TEST_IDLE_SECONDS";
/// `interrupt`, `interrupt,quit`: the signals the helper is deaf to.
#[cfg(unix)]
const IDLE_DEAF: &str = "KOMIQ_POSTGRES_TEST_IDLE_DEAF_TO";
/// A file the helper writes when it is set up.
#[cfg(unix)]
const IDLE_READY: &str = "KOMIQ_POSTGRES_TEST_IDLE_READY_FILE";

/// Not a test: the body of the process that other tests start under another name.
#[test]
#[ignore = "helper process of the tests that need a live process called postgres"]
fn idle_helper() {
  if let Ok(seconds) = std::env::var(IDLE_HELPER) {
    #[cfg(unix)]
    {
      let deaf = std::env::var(IDLE_DEAF).unwrap_or_default();
      for (name, signal) in [("interrupt", libc::SIGINT), ("quit", libc::SIGQUIT)] {
        if deaf.split(',').any(|given| given == name) {
          // SAFETY: SIG_IGN is a valid disposition; nothing else in this helper uses it.
          unsafe { libc::signal(signal, libc::SIG_IGN) };
        }
      }
      if let Some(ready) = std::env::var_os(IDLE_READY) {
        fs::write(ready, "ready").unwrap();
      }
    }
    std::thread::sleep(Duration::from_secs(seconds.parse().unwrap()));
  }
}

/// A live process called `postgres` that stands for a PostgreSQL main process: it runs
/// in `folder` and shows `data_path` as its data directory (`PGDATA`; the test harness
/// takes no `-D` argument, and a real server's `-D` is read in the gate). The exit
/// status arrives on the channel when it ends, so it never stays uncollected.
#[cfg(unix)]
struct StandIn {
  pid: u32,
  ended: std::sync::mpsc::Receiver<std::process::ExitStatus>,
}
#[cfg(unix)]
impl StandIn {
  fn start(program: &Path, folder: &Path, data_path: &Path, deaf_to: &str) -> Self {
    static READY: AtomicUsize = AtomicUsize::new(0);
    let ready = program.with_file_name(format!("ready-{}", READY.fetch_add(1, Ordering::SeqCst)));
    let mut child = crate::child::spawn_when_ready(
      std::process::Command::new(program)
        .args([
          "--ignored",
          "--exact",
          "postgres::tests::idle_helper",
          "--nocapture",
        ])
        .env(IDLE_HELPER, "120")
        .env(IDLE_DEAF, deaf_to)
        .env(IDLE_READY, &ready)
        .env("PGDATA", data_path)
        .current_dir(folder)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null()),
    );
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
      assert!(Instant::now() < deadline, "the stand-in did not start");
      std::thread::sleep(Duration::from_millis(10));
    }
    let (tx, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
      let _ = tx.send(child.wait().unwrap());
    });
    Self { pid, ended }
  }
  /// Write the lock file a PostgreSQL main process keeps in the folder it runs in: its
  /// own process number, then the data directory it acts on.
  /// Its third line is the time of now: the stand-in was created a moment ago, as a
  /// server writes its lock file a moment after it was created.
  fn locked(self, folder: &Path, data_directory: &Path) -> Self {
    let now = std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs() as i64;
    self.locked_at(folder, data_directory, now)
  }
  /// The same with a given start time in the third line.
  fn locked_at(self, folder: &Path, data_directory: &Path, started: i64) -> Self {
    fs::write(
      folder.join("postmaster.pid"),
      format!(
        "{}\n{}\n{started}\n5432\n",
        self.pid,
        data_directory.display()
      ),
    )
    .unwrap();
    self
  }
  fn alive(&self) -> bool {
    process::identity(self.pid).is_ok_and(|seen| seen.is_some())
      && matches!(
        self.ended.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
      )
  }
  /// The signal it ended by, once it has ended.
  fn ended_by(&self) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    self
      .ended
      .recv_timeout(Duration::from_secs(10))
      .expect("the stand-in ended")
      .signal()
  }
  fn kill(self) {
    // SAFETY: the PID is this test's own child, which has not been collected.
    unsafe { libc::kill(self.pid as i32, libc::SIGKILL) };
    let _ = self.ended.recv_timeout(Duration::from_secs(10));
  }
}

/// The real thing: a live process whose program file was deleted. It is never "no
/// process" and never "ended"; with a PostgreSQL name and this data folder as its
/// working folder it is this folder's database.
#[cfg(unix)]
#[test]
fn a_live_process_whose_program_file_was_deleted_is_still_a_live_process() {
  let install = Install::new();
  let place = install.data.parent().unwrap().join("old app é/bin");
  fs::create_dir_all(&place).unwrap();
  let program = place.join("postgres");
  // A copy of this test binary under the name `postgres` (macOS kills a copy of a
  // system program such as `/bin/sleep`), idling in the data folder.
  crate::child::install_test_program(&program);
  let mut child = crate::child::spawn_when_ready(
    std::process::Command::new(&program)
      .args([
        "--ignored",
        "--exact",
        "postgres::tests::idle_helper",
        "--nocapture",
      ])
      .env(IDLE_HELPER, "120")
      .current_dir(&install.data)
      .stdin(Stdio::null())
      .stdout(Stdio::null())
      .stderr(Stdio::null()),
  );
  let pid = child.id();
  let deadline = Instant::now() + Duration::from_secs(10);
  while process::holder(pid) != process::Holder::Program(program.canonicalize().unwrap()) {
    assert!(Instant::now() < deadline, "{:?}", process::holder(pid));
    std::thread::sleep(Duration::from_millis(20));
  }
  fs::remove_dir_all(place.parent().unwrap()).unwrap();
  let seen = process::holder(pid);
  if cfg!(target_os = "macos") {
    assert!(matches!(seen, process::Holder::Gone(_)), "{seen:?}");
  } else {
    // Linux keeps the name and says that the file is deleted.
    let process::Holder::Program(image) = &seen else {
      panic!("{seen:?}")
    };
    assert!(
      image.to_string_lossy().ends_with("postgres (deleted)"),
      "{image:?}"
    );
  }
  let identity = process::identity(pid).unwrap().expect("it runs");
  assert_eq!(identity.pid, pid);
  assert!(
    identity
      .image
      .file_name()
      .unwrap()
      .to_string_lossy()
      .starts_with("postgres"),
    "{identity:?}"
  );
  let file = process::PidFile {
    pid,
    data: install.data.clone(),
    started: None,
  };
  assert_eq!(process::place(pid, &install.data), process::Place::Here);
  // In the decision. (The helper is not started with `-D`; a database is, and the gate
  // does this with a real one. Here the argument is added to what was read.)
  let as_started = |pid| {
    process::identity(pid).map(|read| {
      read.map(|mut read| {
        read.args.push("-D".into());
        read
      })
    })
  };
  let claimed = process::claim(
    &file,
    &install.program,
    &install.data,
    ONE_NAME,
    seen.clone(),
    as_started,
    |pid| process::place(pid, &install.data),
  );
  let found = adopted(claimed);
  assert!(!found.own_program);
  assert_eq!(found.identity.created, identity.created);
  // Without `-D` it is not stopped in place of a main process, and never disregarded.
  let plain = foreign(process::claim(
    &file,
    &install.program,
    &install.data,
    ONE_NAME,
    seen,
    process::identity,
    |pid| process::place(pid, &install.data),
  ));
  assert!(!plain.disregard, "{plain:?}");
  child.kill().unwrap();
  child.wait().unwrap();
}

/// Round 8 (Linux, on a made-up `/proc`): a process that is ending gives up its program
/// link a moment before its state shows `Z`. In that moment its identity was given back
/// as an error ("could not be inspected: No such file or directory"), and the next
/// launch reported a child it had just stopped as one that may still be running. It is
/// a process that has ended. A process without the link that is not ending stays an
/// error, and so does a link that cannot be read for another reason.
#[cfg(unix)]
#[test]
fn linux_a_process_that_is_ending_has_no_identity_and_is_no_error() {
  let root = Scratch::new();
  fs::create_dir_all(root.0.join("sys/kernel/random")).unwrap();
  fs::write(root.0.join("sys/kernel/random/boot_id"), "boot-1\n").unwrap();
  // `flags`: the seventh field after the name. 4194560 is what a running program
  // shows; 4194564 has the bit that says "ending" (4) as well.
  let stat = |pid: u32, state: &str, flags: u64| {
    format!(
      "{pid} (sleep) {state} 1 {pid} {pid} 0 -1 {flags} 100 0 0 0 1 1 0 0 20 0 1 0 5000 1 1\n"
    )
  };
  let entry = |pid: u32, stat: Option<&str>, exe: Option<&str>, cmdline: bool| {
    let dir = root.0.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    if let Some(stat) = stat {
      fs::write(dir.join("stat"), stat).unwrap();
    }
    if let Some(exe) = exe {
      std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
    }
    if cmdline {
      fs::write(dir.join("cmdline"), "sleep\u{0}60\u{0}").unwrap();
    }
  };
  let identity = |pid| process::linux_identity(&root.0, pid);
  // Control: a running process.
  entry(30, Some(&stat(30, "S", 4194560)), Some("/bin/sleep"), true);
  assert_eq!(
    identity(30),
    Ok(Some(process::Identity {
      pid: 30,
      image: "/bin/sleep".into(),
      created: "boot-1:5000".into(),
      args: vec!["sleep".into(), "60".into()],
    }))
  );
  // Ending: marked so, a running state still, and the program link already gone.
  entry(31, Some(&stat(31, "R", 4194564)), None, true);
  assert_eq!(identity(31), Ok(None), "ending, link gone");
  // The same when the arguments are what is gone.
  entry(32, Some(&stat(32, "R", 4194564)), Some("/bin/sleep"), false);
  assert_eq!(identity(32), Ok(None), "ending, arguments gone");
  // Ended and not collected, and the state of a process that is being collected.
  entry(33, Some(&stat(33, "Z", 4194564)), None, false);
  assert_eq!(identity(33), Ok(None));
  entry(34, Some(&stat(34, "X", 4194564)), None, false);
  assert_eq!(identity(34), Ok(None));
  // No entry, and an entry that went away before its first file was read.
  assert_eq!(identity(35), Ok(None));
  entry(36, None, None, false);
  assert_eq!(identity(36), Ok(None));
  // Not ending, and no link (a task of the kernel): unknown, never "no process".
  entry(37, Some(&stat(37, "S", 2129984)), None, true);
  let unknown = identity(37).unwrap_err();
  assert!(
    unknown.starts_with("Cannot inspect process 37: "),
    "{unknown}"
  );
  // Ending, but the link cannot be read for another reason than "not there" (here it
  // is no link at all): an error as well.
  entry(38, Some(&stat(38, "R", 4194564)), None, true);
  fs::write(root.0.join("38/exe"), "not a link").unwrap();
  assert!(identity(38).is_err());

  // Round 9. The name of a program may hold brackets, spaces and letters that look like
  // a state: the fields begin after the LAST closing bracket. Read from the first one,
  // `a) R (b` would give other fields (another start), and the live process `x) Z (y`
  // would be taken for one that has ended.
  let named = |pid: u32, name: &str, state: &str, flags: u64| {
    format!(
      "{pid} ({name}) {state} 1 {pid} {pid} 0 -1 {flags} 100 0 0 0 1 1 0 0 20 0 1 0 5000 1 1\n"
    )
  };
  for (pid, name) in [(40, "a) R (b"), (41, "x) Z (y"), (42, "y) X (z")] {
    entry(
      pid,
      Some(&named(pid, name, "S", 4194560)),
      Some("/bin/sleep"),
      true,
    );
    assert_eq!(
      identity(pid),
      Ok(Some(process::Identity {
        pid,
        image: "/bin/sleep".into(),
        created: "boot-1:5000".into(),
        args: vec!["sleep".into(), "60".into()],
      })),
      "{name}"
    );
  }
  // The state alone says that a process has ended: `Z` and `X` without the ending
  // mark, and with the link and the arguments still readable (as in this made-up tree;
  // entries 33 and 34 above would be answered by the mark as well).
  for (pid, state) in [(43, "Z"), (44, "X")] {
    entry(
      pid,
      Some(&stat(pid, state, 4194560)),
      Some("/bin/sleep"),
      true,
    );
    assert_eq!(identity(pid), Ok(None), "state {state}");
  }
  // And with a name that has brackets: the state after the last bracket decides.
  entry(
    45,
    Some(&named(45, "a) S (b", "Z", 4194560)),
    Some("/bin/sleep"),
    true,
  );
  assert_eq!(identity(45), Ok(None));
}

/// B-3 (Linux, on a made-up `/proc`): a process has ended only when it has no entry or
/// its state is `Z`. A live process without a readable `exe` link is unknown.
#[cfg(unix)]
#[test]
fn linux_a_live_process_without_a_program_link_is_unknown_never_ended() {
  let root = Scratch::new();
  let entry = |pid: u32, stat: Option<&str>, exe: Option<&str>| {
    let dir = root.0.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    if let Some(stat) = stat {
      fs::write(dir.join("stat"), stat).unwrap();
    }
    if let Some(exe) = exe {
      std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
    }
  };
  let holder = |pid| process::linux_holder(&root.0, pid);
  // No entry: no process.
  assert_eq!(holder(10), process::Holder::Free);
  // A live process with its program link.
  entry(
    11,
    Some("11 (postgres) S 1 11 11 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 5000 1 1\n"),
    Some("/opt/pg/bin/postgres"),
  );
  assert_eq!(
    holder(11),
    process::Holder::Program("/opt/pg/bin/postgres".into())
  );
  // The checker's case: alive (state S, R, D, T, I), no `exe` link. Unknown, not ended.
  for (pid, state) in [(12, "S"), (13, "R"), (14, "D"), (15, "T"), (16, "I")] {
    entry(
      pid,
      Some(&format!(
        "{pid} (kworker/0:1-events) {state} 2 0 0 0 -1 69238880 0\n"
      )),
      None,
    );
    let seen = holder(pid);
    assert!(
      matches!(seen, process::Holder::Unreadable(_)),
      "{state}: {seen:?}"
    );
  }
  // Ended and not collected: state Z. Its link is unreadable too, and that does not matter.
  entry(17, Some("17 (postgres) Z 1 17 17 0 -1 4228300 0\n"), None);
  assert_eq!(holder(17), process::Holder::Ended);
  // A name with spaces and parentheses: the state is what follows the LAST `)`.
  entry(
    18,
    Some("18 (a) Z (name) S 1 18 18 0 -1 0 0\n"),
    Some("/usr/bin/odd name"),
  );
  assert_eq!(
    holder(18),
    process::Holder::Program("/usr/bin/odd name".into())
  );
  entry(19, Some("19 (S) R) Z 1 19 19 0 -1 0 0\n"), None);
  assert_eq!(holder(19), process::Holder::Ended);
  // A replaced program file keeps its PostgreSQL name.
  entry(
    20,
    Some("20 (postgres) S 1 20 20 0 -1 0 0\n"),
    Some("/opt/pg/bin/postgres (deleted)"),
  );
  assert_eq!(
    holder(20),
    process::Holder::Program("/opt/pg/bin/postgres (deleted)".into())
  );
  // A state that cannot be read is unknown, never "no process" and never "ended".
  entry(21, Some("garbage"), None);
  assert!(matches!(holder(21), process::Holder::Unreadable(_)));
  entry(22, None, None);
  assert_eq!(
    holder(22),
    process::Holder::Free,
    "no stat file: the entry is going away"
  );

  // In the decision: unknown is never disregarded; ended is.
  let install = Install::new();
  let unknown = foreign(install.claim(holder(12), None, None));
  assert!(!unknown.disregard, "{unknown:?}");
  assert!(foreign(install.claim(holder(17), None, None)).disregard);
}

// ---- a lock file that is not a lock file (F4) -------------------------------------------

/// An empty or garbled `postmaster.pid`, or one that names PID 0 or 1, is never removed
/// or changed and does not stop the start by itself: PostgreSQL is told nothing and
/// decides, and a refusal shows its words, the file and what to do.
#[test]
fn an_empty_or_garbled_lock_file_is_left_alone_and_postgresql_decides() {
  let root = Scratch::new();
  let path = root.0.join("postmaster.pid");
  assert!(matches!(read_lock(&path), LockRead::Absent));
  let binary: &[u8] = &[0xff, 0xfe, 0x00, 0x80];
  for (content, reason) in [
    (&b""[..], "it is empty"),
    (&b"\n"[..], "first two lines"),
    (&b"garbage\n/data\n"[..], "first two lines"),
    (&b"0\n/data\n"[..], "first two lines"),
    (&b"1\n/data\n"[..], "first two lines"),
    (&b"-5\n/data\n"[..], "first two lines"),
    (&b"4242"[..], "first two lines"),
    (&b"4242\nrelative/data\n"[..], "first two lines"),
    (binary, "first two lines"),
  ] {
    fs::write(&path, content).unwrap();
    #[cfg(unix)]
    let inode = {
      use std::os::unix::fs::MetadataExt;
      fs::metadata(&path).unwrap().ino()
    };
    let read = read_lock(&path);
    let LockRead::Unreadable(why) = &read else {
      panic!("{content:?}: {read:?}")
    };
    assert!(why.contains(reason), "{content:?}: {why}");
    assert_eq!(fs::read(&path).unwrap(), content, "never changed");
    #[cfg(unix)]
    {
      use std::os::unix::fs::MetadataExt;
      assert_eq!(fs::metadata(&path).unwrap().ino(), inode, "never replaced");
    }
    // Nothing is said to PostgreSQL about such a file.
    assert_eq!(
      grandparent_pid(Some(&LockNote::Unreadable(why.clone())), false),
      None
    );
  }
  // A real lock file is read as one. Its second line is the data folder as PostgreSQL
  // writes it on the running system: on Windows with a drive and forward slashes, and
  // the lines of the file end in `\r\n` there. (`/data dir é` names no drive, so on
  // Windows it is not a data folder: run 38172246950 failed on exactly that.)
  let (data, end) = if cfg!(windows) {
    ("C:/data dir é", "\r\n")
  } else {
    ("/data dir é", "\n")
  };
  fs::write(
    &path,
    [
      "4242",
      data,
      "1760000000",
      "5432",
      "",
      "127.0.0.1",
      "  1234 5",
      "ready   ",
      "",
    ]
    .join(end),
  )
  .unwrap();
  let LockRead::File(file) = read_lock(&path) else {
    panic!("a full lock file")
  };
  assert_eq!((file.pid, file.data), (4242, PathBuf::from(data)));
  // A path with a root but no drive is not a data folder on Windows.
  if cfg!(windows) {
    fs::write(&path, "4242\r\n/data dir é\r\n1760000000\r\n").unwrap();
    assert!(matches!(read_lock(&path), LockRead::Unreadable(_)));
  }
  // A folder at that name cannot be read as a file: left alone as well.
  fs::remove_file(&path).unwrap();
  fs::create_dir(&path).unwrap();
  assert!(matches!(read_lock(&path), LockRead::Unreadable(_)));

  // What the start page shows when PostgreSQL refuses: PostgreSQL 18.6's own lines for
  // an empty file, then the file and the advice.
  let output = "2026-10-11 14:00:00.000 UTC FATAL:  lock file \"postmaster.pid\" is empty\n\
                2026-10-11 14:00:00.000 UTC HINT:  Either another server is starting, or the lock file is the remnant of a previous server startup crash.";
  let lock = Path::new("/d a/postmaster.pid");
  let text = unreadable_lock_text(output, lock, "it is empty");
  assert!(
    text.starts_with(
      "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" is empty HINT: \
       Either another server is starting, or the lock file is the remnant of a previous \
       server startup crash.\n"
    ),
    "{text}"
  );
  for needle in [
    "The lock file /d a/postmaster.pid is not a usable lock file (it is empty)",
    "did not change or remove it",
    "no `postgres` process is running on this data folder",
    "move this one file aside, for example to /d a/postmaster.pid.aside, and Retry",
  ] {
    assert!(text.contains(needle), "{needle}: {text}");
  }
}

// ---- this folder's earlier database (F1, F2) --------------------------------------------

#[test]
fn the_log_and_the_start_page_name_this_folders_earlier_database() {
  let found = process::Adopted {
    identity: process::Identity {
      pid: 74923,
      image: "/Volumes/Old/Komiq Server.app/Contents/Resources/postgres/bin/postgres".into(),
      created: "1:2".into(),
      args: vec!["postgres".into(), "-D".into(), "/old path/data".into()],
    },
    own_program: false,
  };
  let line = adopted_line(Path::new("/d a/postmaster.pid"), &found);
  for needle in [
    "/d a/postmaster.pid names process 74923 (/Volumes/Old/Komiq Server.app/",
    "this folder's database is still running",
    "its working folder is this data folder",
    "its program is not the one this install starts",
    "asked to stop with pg_ctl",
    "the lock file is not touched",
  ] {
    assert!(line.contains(needle), "{needle}: {line}");
  }
  assert_eq!(line.lines().count(), 1);
  let own = process::Adopted {
    own_program: true,
    ..found.clone()
  };
  assert!(!adopted_line(Path::new("/d a/postmaster.pid"), &own).contains("not the one"));

  // pg_ctl could not stop it: the PID, what it is, and the exact command.
  let text = adopted_stop_failed_text(
    &found,
    Path::new("/d a"),
    StopMode::Fast,
    Some("Bundled pg_ctl failed (exit status: 1). The Database log has the cause."),
  );
  for needle in [
    "Process 74923 (/Volumes/Old/Komiq Server.app/",
    "is this app's earlier database process",
    "still running on /d a",
    "asked it to shut down with pg_ctl and it did not stop (Bundled pg_ctl failed (exit status: 1)",
    "Nothing was forced and the lock file was not removed",
    "wait a minute, then Retry",
    "recovers what was committed at its next start:\nkill -QUIT 74923",
  ] {
    assert!(text.contains(needle), "{needle}: {text}");
  }
  // M5: pg_ctl has already sent the interrupt; the owner is not told to send it again.
  assert!(text.ends_with("kill -QUIT 74923"), "{text}");
  assert!(!text.contains("kill -INT"), "{text}");
  assert!(
    adopted_stop_failed_text(&found, Path::new("/d a"), StopMode::Fast, None)
      .contains("pg_ctl reported success, but the process is still there")
  );
  // After the quit signal was sent and did nothing, the advice is not the quit signal.
  let quit = adopted_stop_failed_text(&found, Path::new("/d a"), StopMode::Immediate, None);
  assert!(
    quit.contains("told it to quit with pg_ctl") && quit.ends_with("kill -KILL 74923"),
    "{quit}"
  );
  assert!(!quit.contains("kill -QUIT"), "{quit}");
}

/// A shutdown writes the control file by the path the server was started with. The
/// earlier database of this folder is asked to shut down, unless that path is another
/// place now and a database lies there: then it is told to quit.
#[test]
fn an_earlier_database_is_told_to_quit_when_its_start_path_holds_another_database() {
  let root = Scratch::new();
  let here = root.0.join("renamed/data");
  let old_path = root.0.join("app data/data");
  for data in [&here, &old_path] {
    fs::create_dir_all(data.join("global")).unwrap();
  }
  fs::write(here.join("global/pg_control"), "this folder's").unwrap();
  let started = |on: &Path| {
    vec![
      "postgres".to_string(),
      "-D".into(),
      on.display().to_string(),
    ]
  };
  // Started on this folder: an ordinary shutdown.
  assert_eq!(adopted_stop_mode(&started(&here), &here), StopMode::Fast);
  // Started on the old path, and nothing is there (or an empty folder): the shutdown
  // cannot write anywhere else. It ends by PostgreSQL's own PANIC; nothing is lost.
  assert_eq!(
    adopted_stop_mode(&started(&old_path), &here),
    StopMode::Fast
  );
  assert_eq!(
    adopted_stop_mode(&started(&root.0.join("missing/data")), &here),
    StopMode::Fast
  );
  // Another database lies at the old path: its control file must not be written.
  fs::write(old_path.join("global/pg_control"), "another database's").unwrap();
  assert_eq!(
    adopted_stop_mode(&started(&old_path), &here),
    StopMode::Immediate
  );
  // The old path is a link to this folder: the same file, an ordinary shutdown.
  #[cfg(unix)]
  {
    let link = root.0.join("link");
    std::os::unix::fs::symlink(&here, &link).unwrap();
    assert_eq!(adopted_stop_mode(&started(&link), &here), StopMode::Fast);
  }
  // No `-D`, or a relative one: nothing is known about another place.
  assert_eq!(
    adopted_stop_mode(&["postgres".into()], &here),
    StopMode::Fast
  );
  assert_eq!(
    adopted_stop_mode(&["postgres".into(), "-D".into(), "data".into()], &here),
    StopMode::Fast
  );
}

// ---- stopping this folder's earlier database: the branches that fail (M5) ------------------

/// A `Database` on a made-up install whose `pg_ctl` is the system's `true` or `false`
/// (a link; no program file is written, so nothing can be "busy"), and a live process
/// that stands for the earlier database: it runs in the data folder and the lock file
/// names it.
#[cfg(unix)]
struct Earlier {
  _root: Scratch,
  database: Database,
  process: std::process::Child,
  found: process::Adopted,
}
#[cfg(unix)]
impl Earlier {
  async fn new(pg_ctl: &str, own_program: bool) -> Self {
    let root = Scratch::new();
    let top = root.0.canonicalize().unwrap();
    let (server, runtime) = (top.join("app data/server"), top.join("runtime"));
    let cluster = cluster_dir(&server);
    let data = cluster.join("data");
    fs::create_dir_all(&data).unwrap();
    fs::create_dir_all(runtime.join("bin")).unwrap();
    let system = ["/usr/bin", "/bin"]
      .iter()
      .map(|dir| Path::new(dir).join(pg_ctl))
      .find(|program| program.exists())
      .expect("the system has `true` and `false`");
    std::os::unix::fs::symlink(system, runtime.join("bin/pg_ctl")).unwrap();
    let process = std::process::Command::new("sleep")
      .arg("300")
      .current_dir(&data)
      .spawn()
      .unwrap();
    let pid = process.id();
    // Right after a spawn Linux may still show this test program for a moment, and then
    // the new program a moment before its arguments.
    let deadline = Instant::now() + Duration::from_secs(10);
    let identity = loop {
      let seen = crate::child::identity_after_spawn(pid)
        .unwrap()
        .expect("it runs")
        .identity;
      if seen.args.last().map(String::as_str) == Some("300") {
        break seen;
      }
      assert!(Instant::now() < deadline, "{seen:?}");
      std::thread::sleep(Duration::from_millis(1));
    };
    fs::write(
      data.join("postmaster.pid"),
      format!("{pid}\n{}\n0\n5432\n", data.display()),
    )
    .unwrap();
    let database = Database {
      runtime: Runtime {
        root: runtime.clone(),
      },
      root: cluster,
      data,
      paths: paths::ToolPaths::new(&server, &runtime).await.unwrap(),
      credentials: Credentials {
        supervisor: "not used".into(),
        application: "not used".into(),
      },
      child: None,
      port: 0,
      log: LogBuf::new(),
    };
    Self {
      _root: root,
      database,
      process,
      found: process::Adopted {
        identity,
        own_program,
      },
    }
  }
  async fn stop(&self) -> Result<(), String> {
    self
      .database
      .stop_adopted_within(&self.found, Duration::from_millis(300))
      .await
  }
  fn running(&mut self) -> bool {
    self.process.try_wait().unwrap().is_none()
  }
  fn end(mut self) {
    let _ = self.process.kill();
    let _ = self.process.wait();
  }
}

/// M5: `pg_ctl` fails, or reports success, and the earlier database is still there. It
/// runs another program than this install's, so nothing is forced: the start is refused
/// with the process, the reason and what to do, and the process and its lock file stay.
#[cfg(unix)]
#[tokio::test]
async fn an_earlier_database_that_pg_ctl_cannot_stop_is_never_forced_when_its_program_is_anothers()
{
  for (pg_ctl, reason) in [
    (
      "false",
      "did not stop (Bundled pg_ctl failed (exit status: 1)",
    ),
    (
      "true",
      "did not stop (pg_ctl reported success, but the process is still there)",
    ),
  ] {
    let mut earlier = Earlier::new(pg_ctl, false).await;
    let pid = earlier.found.identity.pid;
    let error = earlier.stop().await.expect_err("the start is refused");
    for needle in [
      format!("Process {pid} ("),
      "is this app's earlier database process".into(),
      reason.to_string(),
      "Nothing was forced and the lock file was not removed".into(),
    ] {
      assert!(error.contains(&needle), "{pg_ctl}: {needle}: {error}");
    }
    assert!(error.ends_with(&format!("kill -QUIT {pid}")), "{error}");
    assert!(earlier.running(), "{pg_ctl}: the process was not touched");
    assert!(earlier.database.data.join("postmaster.pid").exists());
    assert!(
      !earlier.database.log.text().contains("forcing"),
      "{}",
      earlier.database.log.text()
    );
    earlier.end();
  }
}

/// M5: the same when it runs this install's own program: it is ended by force, by its
/// identity, and the log says that recovery follows.
#[cfg(unix)]
#[tokio::test]
async fn an_earlier_database_of_this_installs_program_is_forced_when_pg_ctl_cannot_stop_it() {
  let mut earlier = Earlier::new("false", true).await;
  // Collect it when it ends, as the system does for a database nobody is the parent of.
  let mut process = std::mem::replace(
    &mut earlier.process,
    std::process::Command::new("true").spawn().unwrap(),
  );
  let (tx, ended) = std::sync::mpsc::channel();
  std::thread::spawn(move || {
    let _ = tx.send(process.wait().unwrap());
  });
  assert_eq!(earlier.stop().await, Ok(()));
  use std::os::unix::process::ExitStatusExt;
  assert_eq!(
    ended
      .recv_timeout(Duration::from_secs(5))
      .expect("the process was ended")
      .signal(),
    Some(libc::SIGKILL)
  );
  let log = earlier.database.log.text();
  assert!(
    log.contains("PostgreSQL fast stop did not complete; forcing the identified postmaster")
      && log.contains("Database required a forced stop; crash recovery runs at next launch"),
    "{log}"
  );
  earlier.end();
}

/// M5: what was looked at is not what is there any more (the lock file names another
/// process, or the process does not run in this folder): nothing is signaled, and
/// `pg_ctl` is not started (the stand-in for it would fail with another message).
#[cfg(unix)]
#[tokio::test]
async fn an_earlier_database_that_changed_since_it_was_looked_at_is_not_signaled() {
  let changed = "changed while it was looked at; nothing was signaled. Retry.";
  // The lock file names another process now.
  let mut earlier = Earlier::new("false", true).await;
  let lock = earlier.database.data.join("postmaster.pid");
  fs::write(&lock, format!("1\n{}\n", earlier.database.data.display())).unwrap();
  let error = earlier.stop().await.expect_err("refused");
  assert!(error.ends_with(changed), "{error}");
  assert!(earlier.running());
  // The lock file is gone.
  fs::remove_file(&lock).unwrap();
  assert!(earlier
    .stop()
    .await
    .expect_err("refused")
    .ends_with(changed));
  // Another process has the number (another creation time).
  fs::write(
    &lock,
    format!(
      "{}\n{}\n",
      earlier.found.identity.pid,
      earlier.database.data.display()
    ),
  )
  .unwrap();
  earlier.found.identity.created.push('0');
  assert!(earlier
    .stop()
    .await
    .expect_err("refused")
    .ends_with(changed));
  assert!(earlier.running());
  assert_eq!(earlier.database.log.text(), "", "pg_ctl was not started");
  earlier.end();

  // The process does not run in this data folder.
  let mut earlier = Earlier::new("false", true).await;
  let mut elsewhere = std::process::Command::new("sleep")
    .arg("300")
    .current_dir(earlier.database.data.parent().unwrap())
    .spawn()
    .unwrap();
  let identity = crate::child::identity_after_spawn(elsewhere.id())
    .unwrap()
    .expect("it runs")
    .identity;
  fs::write(
    earlier.database.data.join("postmaster.pid"),
    format!("{}\n{}\n", identity.pid, earlier.database.data.display()),
  )
  .unwrap();
  earlier.found.identity = identity;
  assert!(earlier
    .stop()
    .await
    .expect_err("refused")
    .ends_with(changed));
  assert!(elsewhere.try_wait().unwrap().is_none(), "not signaled");
  assert_eq!(earlier.database.log.text(), "", "pg_ctl was not started");
  elsewhere.kill().unwrap();
  elsewhere.wait().unwrap();
  earlier.end();
}

// ---- the look before a start: a database of this data path that runs elsewhere (M1) -------

/// The data directory text of a server: `-D`, in both spellings, before `PGDATA`.
#[cfg(unix)]
#[test]
fn the_data_path_of_a_postgresql_is_its_d_argument_or_its_pgdata() {
  let args = |given: &[&str]| -> Vec<String> { given.iter().map(|arg| arg.to_string()).collect() };
  let text = |given: &[&str], pgdata: Option<&str>| {
    process::data_text(&args(given), pgdata.map(str::to_string))
  };
  assert_eq!(
    text(&["/pg/bin/postgres", "-D", "/a b/é", "-p", "5"], None).as_deref(),
    Some("/a b/é")
  );
  assert_eq!(
    text(&["postgres", "-D/joined"], Some("/env")).as_deref(),
    Some("/joined")
  );
  assert_eq!(
    text(&["postgres", "-D", "/first", "-D", "/last"], None).as_deref(),
    Some("/last")
  );
  assert_eq!(
    text(&["postgres", "-p", "5"], Some("/env")).as_deref(),
    Some("/env")
  );
  // The first entry is the program, whatever it is called: never an argument.
  assert_eq!(text(&["-D", "/not an argument"], None), None);
  assert_eq!(
    text(&["-D/not an argument"], Some("/env")).as_deref(),
    Some("/env")
  );
  // A helper process shows a title; an empty text names nothing.
  assert_eq!(text(&["postgres: checkpointer "], None), None);
  assert_eq!(text(&["postgres", "-D"], Some("")), None);
}

/// Which processes the look takes, decided on made-up process lists. Only the lock file
/// in the folder a process runs in proves a database that endangers the data path, and
/// only when its start time is the time that process was created; a name, a `-D`
/// argument, `PGDATA` and a `data_directory` setting can refuse the start and never let
/// a signal go.
#[cfg(unix)]
#[test]
fn the_look_before_a_start_takes_a_postgresql_only_by_the_lock_file_where_it_runs() {
  use process::{Evidence, FolderId, PidFile, Place, Seen, Stray};
  let root = Scratch::new();
  fs::create_dir_all(root.0.join("app data")).unwrap();
  // A new install: the data folder itself is not there yet.
  let data = root.0.join("app data/server/postgres/18/data");
  let link = root.0.join("link");
  std::os::unix::fs::symlink(root.0.join("app data"), &link).unwrap();
  let text = data.display().to_string();
  let by_link = link.join("server/postgres/18/data").display().to_string();
  // Every process was created at second 1000 + its number.
  let seen = |pid: u32, parent, program: &str, data: Option<&str>| Seen {
    pid,
    parent,
    program: program.into(),
    created: format!("1:{pid}"),
    data: data.map(str::to_string),
    data_directory: Vec::new(),
    started: Some(1000 + i64::from(pid)),
    named_by_arguments: false,
  };
  let pg = "/opt/pg/bin/postgres";
  let mut all = vec![
    seen(100, 1, pg, Some(&text)), // main process, runs elsewhere, proven by its lock file
    seen(101, 100, pg, None),      // its helper (a title, no data path)
    seen(102, 100, pg, Some(&text)), // its child that still shows the arguments
    seen(200, 1, pg, Some(&by_link)), // the same place through a link; folder unreadable
    seen(300, 1, pg, Some(&text)), // runs in this folder
    seen(400, 1, pg, Some("/another/data")), // another data path, another lock file
    seen(500, 1, pg, Some("relative/data")), // a relative text cannot be compared
    seen(600, 1, pg, Some(&text)), // gone when its folder is read
    seen(700, 1, pg, None),        // no data path at all, no lock file where it runs
    // A2: programs that only have a PostgreSQL name, with this data path in their
    // environment or arguments. No lock file names them where they run.
    seen(800, 1, "/usr/local/bin/postgres_exporter", Some(&text)),
    seen(810, 1, "/usr/local/bin/postgres-helper", Some(&text)),
    // A3: `-D <this path> -c data_directory=/other/library/data`: a real server of
    // another library, which only reads its configuration from this path.
    seen(900, 1, pg, Some(&text)),
    seen(901, 900, pg, Some(&text)), // its child (PGDATA is inherited)
    // A3, the other way: `-D /separate/config -c data_directory=<this path>`, and the
    // data folder was renamed. Its lock file names it, this path and its start.
    seen(950, 1, pg, Some("/separate/config")),
    seen(951, 950, pg, None),
    // A lock file that names another process (left over from an earlier server).
    seen(960, 1, pg, Some(&text)),
    // The folder it runs in was deleted: it has no name, and no lock file can be read.
    seen(970, 1, pg, Some(&text)),
    // F2: programs with a PostgreSQL name and NO data path in their texts, in a folder
    // whose lock file has their number and this path. The file is what an earlier
    // server left behind (its start is days before this process), or it has no start
    // time at all. Round 6 signaled these.
    seen(1100, 1, "/usr/local/bin/postgres_exporter", None),
    seen(1110, 1, pg, None),
    seen(1120, 1, pg, None),
  ];
  // F2: the time the process was created could not be read.
  all.push(Seen {
    started: None,
    ..seen(1130, 1, pg, None)
  });
  // F4: `-D /separate/config -c data_directory=<this path>` (in each form), and the
  // folder it runs in or the lock file there cannot be read. Round 6 let the start go
  // on beside these: their `-D` does not name this path.
  for (pid, setting) in [
    (
      1200,
      vec!["-c".to_string(), format!("data_directory={text}")],
    ),
    (1210, vec![format!("--data_directory={by_link}")]),
    (1220, vec![format!("-cdata-directory={text}")]),
    (1230, vec![format!("--DATA_DIRECTORY={text}")]),
  ] {
    let mut args = vec![pg.to_string(), "-D".into(), "/separate/config".into()];
    args.extend(setting);
    all.push(Seen {
      data_directory: process::data_directory_texts(&args),
      ..seen(pid, 1, pg, process::data_text(&args, None).as_deref())
    });
  }
  // A2 on Linux: a script `/tmp/postgres -D <this path>` run by an interpreter, and a
  // server under an emulator. Even with a lock file that names it, no signal.
  all.push(Seen {
    named_by_arguments: true,
    ..seen(980, 1, "/tmp/postgres", Some(&text))
  });
  all.push(Seen {
    named_by_arguments: true,
    ..seen(990, 1, pg, Some("/separate/config"))
  });
  let place = |pid| match pid {
    200 | 1200 => Some(Place::Unknown("Operation not permitted".into())),
    300 => Some(Place::Here),
    600 => None,
    970 | 1210 => Some(Place::Elsewhere(None)),
    pid => Some(Place::Elsewhere(Some(format!("/runs/{pid}").into()))),
  };
  let read = |pid: u32, data: &str, started: Option<i64>| Evidence {
    folder: FolderId {
      device: 7,
      inode: u64::from(pid),
    },
    lock: PidFile {
      pid,
      data: data.into(),
      started,
    },
  };
  let lock = |pid: u32, folder: &Path| -> Result<Evidence, String> {
    assert_eq!(folder, Path::new(&format!("/runs/{pid}")), "its own folder");
    // A server writes its start time a moment after it was created.
    let own = |pid: u32, data: &str| Ok(read(pid, data, Some(1000 + i64::from(pid) + 2)));
    match pid {
      100 | 950 | 990 => own(pid, &text),
      // The helpers run in the folder of their main process: its lock file.
      101 | 102 => own(100, &text),
      951 => own(950, &text),
      400 => own(400, "/another/data"),
      900 | 901 => own(900, "/other/library/data"),
      960 => own(4, &text),
      // The script wrote a lock file that names itself: still no proof.
      980 => own(980, &by_link),
      // Left over from a server that was started three days before this process.
      1100 | 1110 => Ok(read(pid, &text, Some(1000 + i64::from(pid) - 3 * 86400))),
      1120 => Ok(read(pid, &text, None)),
      1130 => own(pid, &text),
      _ => Err("No such file or directory".into()),
    }
  };
  let found = process::strays(&data, &all, place, lock);
  let by_pid = |pid: u32| {
    all
      .iter()
      .find(|process| process.pid == pid)
      .unwrap()
      .clone()
  };
  let unproven = |pid: u32, reason: &str| Stray::Unproven {
    process: by_pid(pid),
    reason: reason.into(),
  };
  let no_lock = |pid: u32| {
    unproven(
      pid,
      &format!(
        "the folder it runs in (/runs/{pid}) has no lock file that names it (No such file \
         or directory)"
      ),
    )
  };
  let by_arguments = |pid: u32| {
    unproven(
      pid,
      &format!(
        "the lock file in the folder it runs in (/runs/{pid}) names it as the database of \
         this path, but its program name was read from its arguments and not from the \
         system (it runs through an emulator or an interpreter)"
      ),
    )
  };
  let left_over = |pid: u32| {
    let created = 1000 + i64::from(pid);
    unproven(
      pid,
      &format!(
        "the lock file in the folder it runs in (/runs/{pid}) has its number and this \
         path, but it gives {} (seconds since 1970) as the time its server was started, and \
         this process was created at {created}: the two times do not agree, so the file \
         does not show that this process wrote it",
        created - 3 * 86400
      ),
    )
  };
  let expected = vec![
    Stray::Elsewhere {
      process: by_pid(100),
      folder: "/runs/100".into(),
      evidence: read(100, &text, Some(1102)),
      helpers: vec![by_pid(101), by_pid(102)],
    },
    unproven(
      200,
      "the folder it runs in could not be read (Operation not permitted)",
    ),
    no_lock(800),
    no_lock(810),
    unproven(
      900,
      "the lock file in the folder it runs in (/runs/900) gives /other/library/data as \
       its data directory, not this path",
    ),
    Stray::Elsewhere {
      process: by_pid(950),
      folder: "/runs/950".into(),
      evidence: read(950, &text, Some(1952)),
      helpers: vec![by_pid(951)],
    },
    unproven(
      960,
      "the lock file in the folder it runs in (/runs/960) names process 4, not this one",
    ),
    unproven(
      970,
      "the folder it runs in has no name any more, so no lock file could be read there",
    ),
    left_over(1100),
    left_over(1110),
    unproven(
      1120,
      "the lock file in the folder it runs in (/runs/1120) has its number and this path, \
       but it has no start time in its third line",
    ),
    unproven(
      1130,
      "the lock file in the folder it runs in (/runs/1130) has its number and this path, \
       but it cannot be tied to this process, because the time this process was created \
       could not be read",
    ),
    unproven(
      1200,
      "the folder it runs in could not be read (Operation not permitted)",
    ),
    unproven(
      1210,
      "the folder it runs in has no name any more, so no lock file could be read there",
    ),
    no_lock(1220),
    no_lock(1230),
    by_arguments(980),
    by_arguments(990),
  ];
  assert_eq!(found.len(), expected.len(), "{found:#?}");
  for (found, expected) in found.iter().zip(&expected) {
    assert_eq!(found, expected);
  }
  // The only processes that may ever be signaled: 100 and 950.
  let signaled: Vec<u32> = found
    .iter()
    .filter_map(|stray| match stray {
      Stray::Elsewhere { process, .. } => Some(process.pid),
      Stray::Unproven { .. } => None,
    })
    .collect();
  assert_eq!(signaled, vec![100, 950]);
  // Another data path: nothing ties any of them to it.
  assert_eq!(
    process::strays(Path::new("/nobody/data"), &all, place, lock),
    vec![]
  );
  // A path with parts that do not exist compares by its text below what exists.
  assert_eq!(
    process::resolved(&link.join("server/missing/x")),
    root
      .0
      .canonicalize()
      .unwrap()
      .join("app data/server/missing/x")
  );
}

/// The two small rules of the proof: which arguments are a `data_directory` setting,
/// and when a lock file's start time and a process's creation time are the same start.
#[cfg(unix)]
#[test]
fn data_directory_settings_and_start_times_are_read_strictly() {
  let texts = |args: &[&str]| {
    process::data_directory_texts(&args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
  };
  assert_eq!(
    texts(&["postgres", "-D", "/config", "-c", "data_directory=/a b/é"]),
    vec!["/a b/é"]
  );
  assert_eq!(
    texts(&["postgres", "-cdata_directory=/joined"]),
    vec!["/joined"]
  );
  assert_eq!(
    texts(&["postgres", "--data_directory=/long"]),
    vec!["/long"]
  );
  assert_eq!(
    texts(&["postgres", "--data-directory=/dash"]),
    vec!["/dash"]
  );
  assert_eq!(
    texts(&["postgres", "-c", "Data_Directory=/case"]),
    vec!["/case"]
  );
  assert_eq!(
    texts(&[
      "postgres",
      "-c",
      "data_directory=/one",
      "--data_directory=/two"
    ]),
    vec!["/one", "/two"]
  );
  // Not this setting, no value, or not an argument at all (the program itself).
  for args in [
    &[
      "postgres",
      "-c",
      "port=5432",
      "-c",
      "hba_file=/data_directory=/x",
    ][..],
    &["postgres", "-c", "data_directory="],
    &["postgres", "-c"],
    &["postgres", "data_directory=/bare"],
    &["--data_directory=/the program name"],
    &["postgres: checkpointer "],
  ] {
    assert_eq!(texts(args), Vec::<String>::new(), "{args:?}");
  }

  let agrees = process::start_agrees;
  // PostgreSQL takes the time right after the process was created.
  for after in [-5, -1, 0, 1, 30, 60] {
    assert_eq!(
      agrees(Some(1_760_000_000 + after), Some(1_760_000_000)),
      Ok(()),
      "{after}"
    );
  }
  for after in [-86400, -6, 61, 3600, 86400 * 30] {
    let refusal = agrees(Some(1_760_000_000 + after), Some(1_760_000_000)).unwrap_err();
    assert!(
      refusal.contains("the two times do not agree"),
      "{after}: {refusal}"
    );
  }
  assert!(agrees(None, Some(1)).unwrap_err().contains("no start time"));
  assert!(agrees(Some(1), None)
    .unwrap_err()
    .contains("could not be read"));
  assert!(agrees(None, None).is_err());
  // The third line as PostgreSQL writes it, and what is not a time.
  let third = |text: &str| process::parse_pid(text).unwrap().started;
  assert_eq!(third("23\n/data\n1760000000\n5432\n"), Some(1_760_000_000));
  assert_eq!(third("23\n/data\n 1760000000 \n"), Some(1_760_000_000));
  assert_eq!(third("23\n/data\n"), None);
  assert_eq!(third("23\n/data\nyesterday\n"), None);
}

/// The process list as Linux gives it, read from a made-up `/proc`: PostgreSQL names of
/// this user, with parent, creation time and data path. Also the case of user-mode
/// emulation, where the program link names the emulator.
#[cfg(unix)]
#[test]
fn linux_postgresql_processes_are_read_with_their_data_path() {
  use std::os::unix::fs::MetadataExt;
  let root = Scratch::new();
  let stat = |pid: u32, name: &str, state: &str, parent: u32, started: u32| {
    format!("{pid} ({name}) {state} {parent} 11 11 0 -1 4194560 100 0 0 0 1 1 0 0 20 0 1 0 {started} 1 1\n")
  };
  let entry = |pid: u32, stat: &str, exe: &str, cmdline: &[&str], environ: &[&str]| {
    let dir = root.0.join(pid.to_string());
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("stat"), stat).unwrap();
    std::os::unix::fs::symlink(exe, dir.join("exe")).unwrap();
    let joined = |parts: &[&str]| {
      parts
        .iter()
        .map(|part| format!("{part}\0"))
        .collect::<String>()
    };
    fs::write(dir.join("cmdline"), joined(cmdline)).unwrap();
    fs::write(dir.join("environ"), joined(environ)).unwrap();
  };
  fs::create_dir_all(root.0.join("sys/kernel/random")).unwrap();
  fs::write(root.0.join("sys/kernel/random/boot_id"), "boot-1\n").unwrap();
  let pg = "/opt/pg/bin/postgres";
  entry(
    11,
    &stat(11, "postgres", "S", 1, 5000),
    pg,
    &[pg, "-D", "/a b/data é", "-p", "5"],
    &["HOME=/h"],
  );
  entry(
    12,
    &stat(12, "postgres", "S", 11, 5001),
    pg,
    &["postgres: checkpointer "],
    &[],
  );
  entry(
    13,
    &stat(13, "postgres", "S", 1, 5002),
    pg,
    &[pg],
    &["A=1", "PGDATA=/from env", "B=2"],
  );
  entry(
    14,
    &stat(14, "sleep", "S", 1, 5003),
    "/usr/bin/sleep",
    &["sleep", "-D", "/a b/data é"],
    &[],
  );
  entry(15, &stat(15, "postgres", "Z", 1, 5004), pg, &[], &[]);
  entry(
    16,
    &stat(16, "a) Z (postgres", "R", 7, 5005),
    "/opt/pg/bin/postgres (deleted)",
    &[pg, "-D/joined"],
    &[],
  );
  // Emulation: the link names the emulator; the program is among the first arguments.
  let emulator = "/usr/bin/qemu-aarch64";
  entry(
    17,
    &stat(17, "qemu-aarch64", "S", 1, 5006),
    emulator,
    &[emulator, pg, "-D", "/emulated"],
    &[],
  );
  // An emulated program that is not PostgreSQL stays what the link says; so does one
  // with a PostgreSQL path further back than the first three arguments.
  entry(
    18,
    &stat(18, "qemu-aarch64", "S", 1, 5007),
    emulator,
    &[emulator, "/usr/bin/sleep", "300", pg],
    &[],
  );
  // Round 6, A2: a script called `postgres`, run by an interpreter. It gets a PostgreSQL
  // name from its arguments, and is marked as such: it can refuse a start, and is never
  // signaled (`strays`).
  entry(
    19,
    &stat(19, "python3", "S", 1, 5008),
    "/usr/bin/python3.12",
    &["/usr/bin/python3.12", "/tmp/postgres", "-D", "/a b/data é"],
    &[],
  );
  // Round 7, F4: a separate folder for the configuration; the data is a setting.
  entry(
    20,
    &stat(20, "postgres", "S", 1, 5009),
    pg,
    &[
      pg,
      "-D",
      "/separate/config",
      "-c",
      "data_directory=/set/in the arguments",
    ],
    &[],
  );
  fs::write(
    root.0.join("stat"),
    "cpu  1 2 3\nbtime 1760000000\nprocesses 5\n",
  )
  .unwrap();
  fs::create_dir_all(root.0.join("not-a-pid")).unwrap();

  let me = fs::metadata(&root.0).unwrap().uid();
  let mut seen = process::linux_processes(&root.0, me);
  seen.sort_by_key(|process| process.pid);
  let expect = |pid, parent, program: &str, started: u32, data: Option<&str>| process::Seen {
    pid,
    parent,
    program: program.into(),
    created: format!("boot-1:{started}"),
    data: data.map(str::to_string),
    data_directory: if pid == 20 {
      vec!["/set/in the arguments".to_string()]
    } else {
      Vec::new()
    },
    // The boot (`btime`) plus the ticks after it, at the system's ticks per second.
    // SAFETY: sysconf takes a constant and has no side effect.
    started: Some(
      1_760_000_000 + i64::from(started) / unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as i64,
    ),
    // Only the two whose program name was read from their arguments.
    named_by_arguments: matches!(pid, 17 | 19),
  };
  assert_eq!(
    seen,
    vec![
      expect(11, 1, pg, 5000, Some("/a b/data é")),
      expect(12, 11, pg, 5001, None),
      expect(13, 1, pg, 5002, Some("/from env")),
      expect(
        16,
        7,
        "/opt/pg/bin/postgres (deleted)",
        5005,
        Some("/joined")
      ),
      expect(17, 1, pg, 5006, Some("/emulated")),
      expect(19, 1, "/tmp/postgres", 5008, Some("/a b/data é")),
      expect(20, 1, pg, 5009, Some("/separate/config")),
    ]
  );
  // The two whose name came from their arguments are never taken for signaling, even
  // when a lock file where they run names them and the data path; the real one is.
  let taken = process::strays(
    Path::new("/a b/data é"),
    &seen,
    |_| Some(process::Place::Elsewhere(Some("/runs".into()))),
    |pid, _| {
      Ok(process::Evidence {
        folder: process::FolderId {
          device: 1,
          inode: 2,
        },
        lock: process::PidFile {
          pid,
          data: "/a b/data é".into(),
          // The made-up file agrees with every process: each was started in the same
          // minute of the made-up boot.
          started: Some(1_760_000_050),
        },
      })
    },
  );
  let pids = |signaled: bool| -> Vec<u32> {
    taken
      .iter()
      .filter_map(|stray| match stray {
        process::Stray::Elsewhere { process, .. } => signaled.then_some(process.pid),
        process::Stray::Unproven { process, .. } => (!signaled).then_some(process.pid),
      })
      .collect()
  };
  // (The made-up lock file names whichever process asks, so 12 counts as a main process.)
  assert_eq!(pids(true), vec![11, 12, 13, 16, 20]);
  assert_eq!(pids(false), vec![17, 19]);
  // Processes of another user are not looked at.
  assert_eq!(process::linux_processes(&root.0, me + 1), vec![]);
  // The same rule in the look at one process (the lock file's): an emulated PostgreSQL
  // is a PostgreSQL, never "another program" that PostgreSQL is told to disregard.
  assert_eq!(
    process::linux_holder(&root.0, 17),
    process::Holder::Program(pg.into())
  );
  assert_eq!(
    process::linux_holder(&root.0, 18),
    process::Holder::Program(emulator.into())
  );
  let install = Install::new();
  assert!(
    !foreign(install.claim(
      process::linux_holder(&root.0, 17),
      Some(Err("not read in this test".into())),
      Some(unknown())
    ))
    .disregard
  );
}

/// M1, with live processes. The app data folder is renamed while its database runs, and
/// the app is started again at the old name. The database of the renamed folder still
/// writes its control file by the old path, so it is found by its data path and its
/// working folder and ended before anything is created or started at that path.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn a_postgresql_of_this_data_path_that_runs_elsewhere_is_ended_before_a_start() {
  let root = Scratch::new();
  let top = root.0.canonicalize().unwrap();
  let program = top.join("old app/bin/postgres");
  crate::child::install_test_program(&program);
  let app = top.join("app data é");
  let data = app.join("server/postgres/18/data");
  fs::create_dir_all(&data).unwrap();
  let waits = StrayWaits {
    interrupt: Duration::from_millis(400),
    quit: Duration::from_secs(10),
  };
  let look = || look_for_strays(&data);

  // This folder's own database (it runs in this folder) is none of this look's business.
  let own = StandIn::start(&program, &data, &data, "").locked(&data, &data);
  assert_eq!(look(), vec![]);
  let log = LogBuf::new();
  assert_eq!(end_strays(&data, &log, waits).await, Ok(()));
  assert!(own.alive() && log.text().is_empty(), "{}", log.text());

  // The folder is renamed aside. Nothing lies at the data path now.
  let aside = top.join("app data é aside");
  fs::rename(&app, &aside).unwrap();
  let aside_data = aside.join("server/postgres/18/data");
  let found = look();
  let [process::Stray::Elsewhere {
    process: seen,
    folder,
    helpers,
    ..
  }] = found.as_slice()
  else {
    panic!("{found:?}")
  };
  assert_eq!(
    (seen.pid, seen.data.as_deref()),
    (own.pid, Some(data.to_str().unwrap()))
  );
  assert_eq!(seen.program, program);
  assert_eq!(folder, &aside_data);
  assert!(helpers.is_empty());
  // Nothing at the path: asked to shut down, and it ends by the interrupt.
  assert_eq!(end_strays(&data, &log, waits).await, Ok(()));
  assert_eq!(own.ended_by(), Some(libc::SIGINT));
  let text = log.text();
  for needle in [
    format!("Process {} ({}) is a PostgreSQL that was started on this data path ({})", own.pid, program.display(), data.display()),
    format!("now runs in another folder ({})", aside_data.display()),
    "The lock file in the folder it runs in names this process, this data path and the time this process was started.".into(),
    "It is ended before anything is created or started here: it is asked to shut down (the interrupt signal)".into(),
    format!("Process {} ({}) has ended (signals sent: interrupt)", own.pid, program.display()),
  ] {
    assert!(text.contains(&needle), "{needle}: {text}");
  }
  assert_eq!(look(), vec![]);

  // A new database was made at the path meanwhile. A shutdown of the old server would
  // write this control file, so the interrupt is never sent: it ends by the quit signal.
  // (This stand-in would end by the interrupt if it got one.)
  fs::create_dir_all(data.join("global")).unwrap();
  fs::write(data.join("global/pg_control"), "the new database's").unwrap();
  let old = StandIn::start(&program, &aside_data, &data, "").locked(&aside_data, &data);
  let log = LogBuf::new();
  assert_eq!(end_strays(&data, &log, waits).await, Ok(()));
  assert_eq!(old.ended_by(), Some(libc::SIGQUIT));
  let text = log.text();
  assert!(
    text.contains("it is told to quit at once (the quit signal) and not asked to shut down")
      && text.contains("has ended (signals sent: quit)"),
    "{text}"
  );
  assert_eq!(
    fs::read_to_string(data.join("global/pg_control")).unwrap(),
    "the new database's"
  );

  // Nothing at the path again, and a server that does not answer the interrupt: after
  // the bounded wait it is told to quit.
  fs::remove_dir_all(data.join("global")).unwrap();
  let slow = StandIn::start(&program, &aside_data, &data, "interrupt").locked(&aside_data, &data);
  let log = LogBuf::new();
  let asked = Instant::now();
  assert_eq!(end_strays(&data, &log, waits).await, Ok(()));
  assert!(asked.elapsed() >= waits.interrupt, "{:?}", asked.elapsed());
  assert_eq!(slow.ended_by(), Some(libc::SIGQUIT));
  assert!(
    log
      .text()
      .contains("has ended (signals sent: interrupt, then quit)"),
    "{}",
    log.text()
  );

  // One that answers neither: the start is refused with its process number, and it is
  // never ended by force.
  let deaf =
    StandIn::start(&program, &aside_data, &data, "interrupt,quit").locked(&aside_data, &data);
  let short = StrayWaits {
    interrupt: Duration::from_millis(200),
    quit: Duration::from_millis(400),
  };
  let error = end_strays(&data, &LogBuf::new(), short)
    .await
    .expect_err("the start is refused");
  for needle in [
    format!("Process {} ({})", deaf.pid, program.display()),
    "It was told to quit and has not ended".into(),
    "the app created and started nothing here".into(),
    format!("kill -KILL {}", deaf.pid),
  ] {
    assert!(error.contains(&needle), "{needle}: {error}");
  }
  assert!(deaf.alive());

  // One whose working folder cannot be read: never signaled, the start is refused with
  // its process number. (A process of this user can always be read here, so the look's
  // answer is made up for a live process.)
  let unread = process::Stray::Unproven {
    process: process::Seen {
      pid: deaf.pid,
      parent: 1,
      program: program.clone(),
      created: process::identity(deaf.pid).unwrap().unwrap().created,
      data: Some(data.display().to_string()),
      data_directory: Vec::new(),
      started: None,
      named_by_arguments: false,
    },
    reason: "the folder it runs in could not be read (Operation not permitted)".into(),
  };
  let error = end_found(&data, &LogBuf::new(), waits, vec![unread])
    .await
    .expect_err("the start is refused");
  for needle in [
    format!("Process {} ({})", deaf.pid, program.display()),
    "the folder it runs in could not be read (Operation not permitted)".into(),
    "the app created and started nothing here".into(),
    format!("It did not signal process {}", deaf.pid),
    format!(
      "End process {} yourself, or restart this computer, then Retry.",
      deaf.pid
    ),
  ] {
    assert!(error.contains(&needle), "{needle}: {error}");
  }
  assert!(deaf.alive());
  deaf.kill();
  assert_eq!(look(), vec![]);

  // ---- Round 6, A2 and A3: what is NOT a proof -------------------------------------------
  // Each of these was signaled by the look of round 5 (it took a PostgreSQL name and a
  // data path text for proof). Now none is signaled: the start is refused, and the
  // message names the process.
  let refused = |error: &str, process: &StandIn, program: &Path, why: &str| {
    for needle in [
      format!("Process {} ({})", process.pid, program.display()),
      "has the name of a PostgreSQL program and has to do with this data path".into(),
      why.to_string(),
      format!("It did not signal process {}", process.pid),
      format!("End process {} yourself", process.pid),
    ] {
      assert!(error.contains(&needle), "{needle}: {error}");
    }
    assert!(process.alive(), "it was not signaled");
  };
  let elsewhere = top.join("somewhere else é");
  fs::create_dir_all(&elsewhere).unwrap();
  // A2: programs that are not PostgreSQL and only have such a name, with this data path
  // in their environment. (This stand-in takes no `-D`; `data_text` reads both the same
  // way, and the made-up lists above have the `-D` forms.)
  for name in ["postgres_exporter", "postgres-helper"] {
    let tool = top.join("tools").join(name);
    crate::child::install_test_program(&tool);
    let running = StandIn::start(&tool, &elsewhere, &data, "");
    let error = end_strays(&data, &LogBuf::new(), waits)
      .await
      .expect_err("the start is refused");
    refused(&error, &running, &tool, "has no lock file that names it");
    // Not even a lock file that another process left in that folder makes it a database.
    fs::write(
      elsewhere.join("postmaster.pid"),
      format!("{}\n{}\n", own.pid, data.display()),
    )
    .unwrap();
    let error = end_strays(&data, &LogBuf::new(), waits)
      .await
      .expect_err("the start is refused");
    refused(
      &error,
      &running,
      &tool,
      &format!("names process {}, not this one", own.pid),
    );
    fs::remove_file(elsewhere.join("postmaster.pid")).unwrap();
    running.kill();
  }
  // A3: `-D <this path> -c data_directory=<another library>`. A real server of another
  // library: it runs in that library's folder, and its lock file there gives that folder
  // as its data directory. This path only holds its configuration files.
  let library = top.join("another library é/data");
  fs::create_dir_all(&library).unwrap();
  let configured = StandIn::start(&program, &library, &data, "").locked(&library, &library);
  let error = end_strays(&data, &LogBuf::new(), waits)
    .await
    .expect_err("the start is refused");
  refused(
    &error,
    &configured,
    &program,
    &format!(
      "gives {} as its data directory, not this path",
      library.display()
    ),
  );
  configured.kill();
  // A3 the other way: `-D <a configuration folder> -c data_directory=<this path>`, and
  // the data folder was renamed. Its texts name the configuration folder only; the lock
  // file where it runs names it and this path. The look of round 5 did not see it.
  let config = top.join("separate config");
  fs::create_dir_all(&config).unwrap();
  let overridden = StandIn::start(&program, &aside_data, &config, "").locked(&aside_data, &data);
  let found = look();
  assert!(
    matches!(found.as_slice(), [process::Stray::Elsewhere { process, .. }]
      if process.pid == overridden.pid && process.data.as_deref() == config.to_str()),
    "{found:?}"
  );
  assert_eq!(end_strays(&data, &LogBuf::new(), waits).await, Ok(()));
  assert_eq!(overridden.ended_by(), Some(libc::SIGINT));
  assert_eq!(look(), vec![]);

  // ---- Round 7, F2: a lock file that is left over, and a process that got its number --------
  // A program that is not PostgreSQL (and one that is called `postgres`) runs in a folder
  // whose lock file has its very process number and this data path. Nothing in its
  // arguments or environment names this path. The file was written by an earlier server
  // that had this number: its start time is three days before this process was created.
  // Round 6 took the file for proof and signaled the process.
  let now = || {
    std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap()
      .as_secs() as i64
  };
  let exporter = top.join("tools").join("postgres_exporter");
  for image in [&exporter, &program] {
    let reused = StandIn::start(image, &elsewhere, &elsewhere, "").locked_at(
      &elsewhere,
      &data,
      now() - 3 * 86400,
    );
    let error = end_strays(&data, &LogBuf::new(), waits)
      .await
      .expect_err("the start is refused");
    refused(
      &error,
      &reused,
      image,
      "the two times do not agree, so the file does not show that this process wrote it",
    );
    // No start time in the file at all: no proof either.
    fs::write(
      elsewhere.join("postmaster.pid"),
      format!("{}\n{}\n", reused.pid, data.display()),
    )
    .unwrap();
    let error = end_strays(&data, &LogBuf::new(), waits)
      .await
      .expect_err("the start is refused");
    refused(
      &error,
      &reused,
      image,
      "has no start time in its third line",
    );
    fs::remove_file(elsewhere.join("postmaster.pid")).unwrap();
    // Without a lock file nothing ties it to this path: none of this app's business.
    assert_eq!(look(), vec![]);
    reused.kill();
  }

  // ---- Round 7, F3: the proof is made again right before every signal ------------------------
  let changed = |error: &str, process: &StandIn, why: &str| {
    for needle in [
      format!("Process {} ({})", process.pid, program.display()),
      "when the app was about to signal it, that was no longer proven".into(),
      why.to_string(),
      "The app sent it nothing at that moment".into(),
      format!("End process {} yourself", process.pid),
    ] {
      assert!(error.contains(&needle), "{needle}: {error}");
    }
    assert!(process.alive(), "it was not signaled");
  };
  let lock_aside = aside_data.join("postmaster.pid");
  // (The grace for a server that is just ending is two seconds; these never end.)
  // 1. The lock file is gone between the look and the signal.
  let stale = StandIn::start(&program, &aside_data, &data, "").locked(&aside_data, &data);
  let found = look();
  assert!(
    matches!(found.as_slice(), [process::Stray::Elsewhere { process, .. }] if process.pid == stale.pid),
    "{found:?}"
  );
  let kept = fs::read(&lock_aside).unwrap();
  fs::remove_file(&lock_aside).unwrap();
  let error = end_found(&data, &LogBuf::new(), waits, found.clone())
    .await
    .expect_err("the start is refused");
  changed(&error, &stale, "postmaster.pid");
  // 2. The lock file has other content: the same number and path, another start time.
  fs::write(
    &lock_aside,
    format!("{}\n{}\n{}\n", stale.pid, data.display(), now() + 30),
  )
  .unwrap();
  let error = end_found(&data, &LogBuf::new(), waits, found.clone())
    .await
    .expect_err("the start is refused");
  changed(&error, &stale, "is not what it was a moment ago");
  // 3. Another folder is at the path the process was seen in (its own was renamed).
  fs::write(&lock_aside, &kept).unwrap();
  let renamed_again = aside.join("server/postgres/18/data renamed again");
  fs::rename(&aside_data, &renamed_again).unwrap();
  fs::create_dir(&aside_data).unwrap();
  fs::write(&lock_aside, &kept).unwrap();
  let error = end_found(&data, &LogBuf::new(), waits, found.clone())
    .await
    .expect_err("the start is refused");
  changed(
    &error,
    &stale,
    "is not the folder the process runs in any more",
  );
  fs::remove_dir_all(&aside_data).unwrap();
  fs::rename(&renamed_again, &aside_data).unwrap();
  // Control: with everything as it was at the look, the same evidence ends it.
  assert_eq!(end_found(&data, &LogBuf::new(), waits, found).await, Ok(()));
  assert_eq!(stale.ended_by(), Some(libc::SIGINT));

  // 4. Between the interrupt and the quit signal: a process that does not answer the
  //    interrupt, and whose lock file goes away while the app waits. The quit signal
  //    must not be sent on the proof of the first look.
  let slow = StandIn::start(&program, &aside_data, &data, "interrupt").locked(&aside_data, &data);
  let remover = {
    let lock_aside = lock_aside.clone();
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(150));
      fs::remove_file(lock_aside).unwrap();
    })
  };
  let error = end_strays(&data, &LogBuf::new(), waits)
    .await
    .expect_err("the start is refused");
  remover.join().unwrap();
  changed(&error, &slow, "postmaster.pid");
  slow.kill();

  // The lock file read itself: only from the folder the process runs in.
  let other = StandIn::start(&program, &elsewhere, &elsewhere, "").locked(&aside_data, &data);
  let error = process::lock_where_it_runs(other.pid, &aside_data).unwrap_err();
  assert!(
    error.contains("is not the folder the process runs in any more"),
    "{error}"
  );
  other.kill();
}

#[test]
fn a_refused_start_shows_postgresqls_words_and_the_process() {
  let foreign = process::Foreign {
    pid: 74923,
    image: Some("/opt/pg/bin/postgres".into()),
    what: "a PostgreSQL process whose data folder is not known".into(),
    disregard: false,
  };
  // The two lines PostgreSQL 18.6 printed for a lock file naming a live process.
  let output = "2026-10-11 13:18:52.905 UTC FATAL:  lock file \"postmaster.pid\" already exists\n\
                2026-10-11 13:18:52.905 UTC HINT:  Is another postmaster (PID 74923) running in data directory \"/d\"?";
  let text = refusal_text(output, Path::new("/d/postmaster.pid"), &foreign);
  assert!(
    text.starts_with(
      "PostgreSQL did not start. It said: FATAL: lock file \"postmaster.pid\" already exists \
       HINT: Is another postmaster (PID 74923) running in data directory \"/d\"?\n"
    ),
    "{text}"
  );
  assert!(
    text.contains("names process 74923 (/opt/pg/bin/postgres)")
      && text.contains("was not stopped")
      && text.contains("was not removed")
      && text.contains("then Retry"),
    "{text}"
  );
  assert!(refusal_text("", Path::new("/d/postmaster.pid"), &foreign).contains("gave no reason"));
}

// ---- major version (round 3 item 10) -------------------------------------------------------

fn fake_cluster(postgres_dir: &Path, folder: &str, version: &str) -> PathBuf {
  let data = postgres_dir.join(folder).join("data");
  fs::create_dir_all(&data).unwrap();
  fs::write(data.join("PG_VERSION"), format!("{version}\n")).unwrap();
  fs::write(data.join("precious"), "the owner's library").unwrap();
  data
}

#[test]
fn a_cluster_of_another_major_version_blocks_the_start_and_is_kept() {
  let root = Scratch::new();
  let postgres_dir = root.0.join("server/postgres");
  // No folder yet (a new install), an empty one, and a folder that is not a cluster.
  assert_eq!(check_major(&postgres_dir, "18"), Ok(()));
  fs::create_dir_all(postgres_dir.join("18")).unwrap();
  fs::write(postgres_dir.join("notes.txt"), "not a folder").unwrap();
  assert_eq!(check_major(&postgres_dir, "18"), Ok(()));
  let data = fake_cluster(&postgres_dir, "18", "18");
  assert_eq!(check_major(&postgres_dir, "18"), Ok(()));

  // A later app whose runtime is PostgreSQL 19 meets this install's folder.
  let error = check_major(&postgres_dir, "19").unwrap_err();
  for needle in [
    data.display().to_string().as_str(),
    "PostgreSQL 18;",
    "runs PostgreSQL 19",
    "The data is kept",
    "no new empty database was made",
    "newer version of the app with a migration step is needed",
  ] {
    assert!(error.contains(needle), "{needle}: {error}");
  }
  assert!(
    !postgres_dir.join("19").exists(),
    "nothing created beside it"
  );

  // This app meets the folder of a later one (the app was downgraded), under that
  // version's own folder name, with or without a folder of this version beside it.
  let later = fake_cluster(&postgres_dir, "19", "19");
  let error = check_major(&postgres_dir, "18").unwrap_err();
  assert!(error.contains(&later.display().to_string()), "{error}");
  fs::remove_dir_all(postgres_dir.join("18")).unwrap();
  let error = check_major(&postgres_dir, "18").unwrap_err();
  assert!(error.contains("PostgreSQL 19;"), "{error}");
  assert!(!postgres_dir.join("18").exists());

  // A folder named for this version that holds another version's cluster.
  fs::remove_dir_all(postgres_dir.join("19")).unwrap();
  let wrong = fake_cluster(&postgres_dir, "18", "17");
  let error = check_major(&postgres_dir, "18").unwrap_err();
  assert!(
    error.contains(&wrong.display().to_string()) && error.contains("PostgreSQL 17;"),
    "{error}"
  );
  assert_eq!(check_cluster(&wrong).unwrap_err(), error, "the same words");
  assert_eq!(
    fs::read_to_string(wrong.join("precious")).unwrap(),
    "the owner's library"
  );
}

/// The start itself: with an older cluster in the data folder, `prepare` stops before it
/// creates this version's folder, credentials or cluster, and before it looks for the
/// runtime (the runtime path given here does not exist).
#[test]
fn prepare_refuses_another_major_version_before_it_creates_anything() {
  let root = Scratch::new();
  let server = root.0.join("server");
  let older = fake_cluster(&server.join("postgres"), "17", "17");
  let before: Vec<_> = fs::read_dir(server.join("postgres"))
    .unwrap()
    .map(|e| e.unwrap().file_name())
    .collect();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
  let log = LogBuf::new();
  let result = runtime.block_on(Database::prepare(
    &root.0.join("no runtime here"),
    &server,
    std::slice::from_ref(&log),
  ));
  let error = result.err().expect("blocked");
  // The folder is named in the form the app works with and shows: on Windows the plain
  // `C:\…` form, not the `\\?\C:\…` form that `canonicalize` gives there (PostgreSQL's
  // tools do not take that form, and a person cannot paste it into Explorer). Run
  // 38172246950 failed here because this test looked for the `\\?\` form.
  let shown = paths::tool_path(older.canonicalize().unwrap())
    .display()
    .to_string();
  assert!(!shown.starts_with(r"\\?\"), "{shown}");
  assert!(
    error.contains(&shown)
      && !error.contains(r"\\?\")
      && error.contains("PostgreSQL 17;")
      && error.contains("The data is kept"),
    "{error}"
  );
  let after: Vec<_> = fs::read_dir(server.join("postgres"))
    .unwrap()
    .map(|e| e.unwrap().file_name())
    .collect();
  assert_eq!(
    after, before,
    "no folder of this version was made beside it"
  );
  assert!(!cluster_dir(&server).exists());
  assert_eq!(
    fs::read_to_string(older.join("precious")).unwrap(),
    "the owner's library"
  );
}

// ---- retention with dumps dated in the future (A6) -------------------------------------------

/// The clock ran a year ahead for five nights and was then corrected. The five dumps
/// with the wrong dates must not push out the correctly dated ones.
#[test]
fn retention_prefers_correctly_dated_dumps_over_future_dated_ones() {
  let now = utc("2026-10-11T09:00:00Z");
  let root = Scratch::new();
  let mut ahead = Vec::new();
  for day in 1..=5 {
    ahead.push(write_dump(
      &root.0,
      &format!("2027-10-{day:02}T02:00:00Z"),
      "ffffffff",
    ));
  }
  let mut dated = Vec::new();
  for day in 1..=4 {
    dated.push(write_dump(
      &root.0,
      &format!("2026-10-{day:02}T02:00:00Z"),
      "aaaaaaaa",
    ));
  }
  // Inside the tolerance: an ordinary dump, not a future-dated one.
  let near = write_dump(&root.0, "2026-10-11T09:04:59Z", "cccccccc");
  let fresh = write_dump(&root.0, "2026-10-11T09:00:00Z", "bbbbbbbb");
  // 1 new + 5 correctly dated + 5 future-dated = 11. Seven stay: the new one, the five
  // correctly dated ones, and one future-dated dump for the place that is left (the
  // newest of them).
  let mut removed = retain_dumps(&root.0, &fresh, now).unwrap();
  removed.sort();
  assert_eq!(removed, ahead[..4].to_vec(), "four future-dated dumps go");
  for kept in dated.iter().chain([&near, &fresh, &ahead[4]]) {
    assert!(kept.exists(), "{}", kept.display());
  }
  assert_eq!(dumps(&root.0).unwrap().len(), DUMP_KEEP);

  // Two more nights with the right clock: the last future-dated dump goes before any
  // correctly dated one does.
  let now = utc("2026-10-12T09:00:00Z");
  let second = write_dump(&root.0, "2026-10-12T09:00:00Z", "dddddddd");
  assert_eq!(
    retain_dumps(&root.0, &second, now).unwrap(),
    vec![ahead[4].clone()]
  );
  let now = utc("2026-10-13T09:00:00Z");
  let third = write_dump(&root.0, "2026-10-13T09:00:00Z", "eeeeeeee");
  assert_eq!(
    retain_dumps(&root.0, &third, now).unwrap(),
    vec![dated[0].clone()],
    "then the oldest correctly dated one"
  );
  // The dump just written stays even when the clock says it is itself in the future
  // (the clock was set back between the write and this call).
  let early = utc("2026-01-01T00:00:00Z");
  let fourth = write_dump(&root.0, "2026-10-14T09:00:00Z", "99999999");
  let removed = retain_dumps(&root.0, &fourth, early).unwrap();
  assert!(fourth.exists() && !removed.contains(&fourth));
  assert_eq!(dumps(&root.0).unwrap().len(), DUMP_KEEP);
}

// ---- the dump task's decision path (round 3 item 7) -----------------------------------------

/// Drives the same `DumpTicker::tick` that `Database::dump_task` calls every 30 seconds,
/// with an injected clock and a dump runner that writes a file the way the real one does.
#[test]
fn the_dump_task_dumps_when_due_once_per_interval_and_retries_a_failure_hourly() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let log = LogBuf::new();
  let runtime = tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap();
  let started = Instant::now();
  let made = std::cell::Cell::new(0u32);
  // One look of the task, `minutes` after it started. `fail`: the dump tool fails.
  let look = |ticker: &mut DumpTicker, minutes: u64, fail: bool| {
    let now = utc("2026-10-11T22:00:00Z") + chrono::Duration::minutes(minutes as i64);
    let at = started + Duration::from_secs(minutes * 60);
    let dir = backups.clone();
    let made = &made;
    runtime.block_on(ticker.tick(now, DUMP_POLL, at, &log, || async move {
      made.set(made.get() + 1);
      if fail {
        return Err("pg_dump: connection refused".to_string());
      }
      let path = dir.join(dump_name(now, &format!("{:08}", made.get())));
      fs::write(&path, "successful dump").map_err(|e| e.to_string())?;
      Ok(path)
    }))
  };
  let mut ticker = DumpTicker::new(backups.clone());

  // No dump in the folder: due at the first look. The tool fails.
  assert!(look(&mut ticker, 0, true), "a due dump is attempted");
  assert_eq!(made.get(), 1);
  assert!(
    log
      .text()
      .contains("Nightly dump failed: pg_dump: connection refused"),
    "{}",
    log.text()
  );
  // Still due, but a failed attempt is repeated after an hour, not at every look.
  for minutes in [1, 30, 59] {
    assert!(!look(&mut ticker, minutes, false), "minute {minutes}");
  }
  assert_eq!(made.get(), 1);
  assert!(
    look(&mut ticker, 60, false),
    "an hour later it is tried again"
  );
  assert_eq!(made.get(), 2);
  assert_eq!(dumps(&backups).unwrap().len(), 1);
  // A good dump keeps the task quiet for 20 hours of looks.
  for minutes in (61..60 + 20 * 60).step_by(7) {
    assert!(!look(&mut ticker, minutes, false), "minute {minutes}");
  }
  assert_eq!(made.get(), 2);
  assert!(
    look(&mut ticker, 60 + 20 * 60, false),
    "the next night's dump"
  );
  assert_eq!(made.get(), 3);
  assert_eq!(dumps(&backups).unwrap().len(), 2);
  // A restarted app (a new ticker) does not dump again either.
  let mut restarted = DumpTicker::new(backups.clone());
  assert!(!look(&mut restarted, 61 + 20 * 60, false));
  assert_eq!(made.get(), 3);
}

/// The dump runner of the loop tests: a clock that only its own pauses move, and a dump
/// that (with `fail_first`) fails the first time and then writes a file the way the real
/// one does.
struct ScriptedDumps {
  started: (DateTime<Utc>, Instant),
  elapsed: Duration,
  end_after: Duration,
  fail_first: bool,
  dir: PathBuf,
  /// Seconds after the start at which the loop asked for a dump.
  asked: std::rc::Rc<std::cell::RefCell<Vec<u64>>>,
  pauses: std::rc::Rc<std::cell::Cell<u64>>,
  done: Option<tokio::sync::oneshot::Sender<()>>,
}
impl DumpRun for ScriptedDumps {
  fn now(&mut self) -> (DateTime<Utc>, Instant) {
    (
      self.started.0 + chrono::Duration::from_std(self.elapsed).unwrap(),
      self.started.1 + self.elapsed,
    )
  }
  async fn pause(&mut self, time: Duration) {
    assert_eq!(time, DUMP_POLL, "the loop looks every DUMP_POLL");
    self.pauses.set(self.pauses.get() + 1);
    self.elapsed += time;
    if self.elapsed > self.end_after {
      if let Some(done) = self.done.take() {
        let _ = done.send(());
      }
      std::future::pending::<()>().await;
    }
  }
  async fn dump(&mut self) -> Result<PathBuf, String> {
    let first = self.asked.borrow().is_empty();
    self.asked.borrow_mut().push(self.elapsed.as_secs());
    if first && self.fail_first {
      return Err("pg_dump: connection refused".into());
    }
    let (wall, _) = self.now();
    let path = self.dir.join(dump_name(
      wall,
      &format!("{:08}", self.asked.borrow().len()),
    ));
    fs::write(&path, "successful dump").map_err(|e| e.to_string())?;
    Ok(path)
  }
}

/// Run `dump_loop` on `backups` for 45 hours of the scripted clock. Returns the seconds
/// after the start at which it asked for a dump, the number of its pauses, and its log.
fn run_dump_loop(backups: &Path, fail_first: bool) -> (Vec<u64>, u64, String) {
  let log = LogBuf::new();
  let asked = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
  let pauses = std::rc::Rc::new(std::cell::Cell::new(0));
  let (done, finished) = tokio::sync::oneshot::channel();
  let run = ScriptedDumps {
    started: (utc("2026-10-11T22:00:00Z"), Instant::now()),
    elapsed: Duration::ZERO,
    end_after: 45 * HOUR,
    fail_first,
    dir: backups.to_path_buf(),
    asked: asked.clone(),
    pauses: pauses.clone(),
    done: Some(done),
  };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .build()
    .unwrap();
  runtime.block_on(async {
    tokio::select! {
      _ = dump_loop(backups.to_path_buf(), log.clone(), run) => unreachable!("the loop never ends"),
      _ = finished => {}
    }
  });
  let asked = asked.borrow().clone();
  (asked, pauses.get(), log.text())
}

/// The loop gives the schedule the time the app has run between two looks. With a newest
/// dump dated in the future (the clock was set back) that running time is the only thing
/// that makes the next dump due: 20 hours of it, then every 20 hours.
#[test]
fn the_dump_task_loop_counts_running_time_for_a_dump_dated_in_the_future() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let ahead = backups.join(dump_name(utc("2030-01-01T00:00:00Z"), "0000ahead"));
  fs::write(&ahead, "a dump made before the clock was set back").unwrap();
  let (asked, _, log) = run_dump_loop(&backups, false);
  assert_eq!(asked, vec![20 * 3600, 40 * 3600], "{log}");
  assert!(ahead.exists(), "the dump dated in the future is kept");
  assert!(log.contains("dated in the future"), "{log}");
}

/// Round 4, 8c: the loop that `Database::dump_task` spawns (`dump_loop`), not only
/// `DumpTicker::tick`, with an injected clock and dump runner. 45 hours of looks.
#[test]
fn the_dump_task_loop_looks_every_poll_and_dumps_on_schedule() {
  let root = Scratch::new();
  let backups = root.0.join("backups");
  fs::create_dir(&backups).unwrap();
  let (asked, pauses, log) = run_dump_loop(&backups, true);
  // At once (no dump in the folder; the tool fails), an hour later (the retry of a
  // failure), then every 20 hours after a good dump: 21 h and 41 h.
  assert_eq!(
    asked,
    vec![0, 3600, 21 * 3600, 41 * 3600],
    "when the loop asked for a dump, in seconds after its start"
  );
  assert_eq!(pauses, 45 * 3600 / DUMP_POLL.as_secs() + 1);
  assert_eq!(dumps(&backups).unwrap().len(), 3, "three good dumps");
  assert_eq!(log.matches("Nightly dump failed").count(), 1, "{log}");
}

// ---- Windows facts read in fix round 6 ----------------------------------------------------------

/// PostgreSQL 18.6's own words when it is started with administrator rights (Windows;
/// read from `postgres.exe`) or as root (Unix): the start page names the cause and what
/// to do, instead of "PostgreSQL exited while starting". Any other output is not taken
/// for this.
#[test]
fn a_start_refused_for_the_rights_of_the_app_says_so() {
  let data = Path::new("/d a é/data");
  let windows = "Execution of PostgreSQL by a user with administrative permissions is not\n\
                 permitted.\n\
                 The server must be started under an unprivileged user ID to prevent\n\
                 possible system security compromises.  See the documentation for\n\
                 more information on how to properly start the server.\n";
  // As the pump of the failed start hands it on: with `\r\n` line ends as well.
  for output in [windows.to_string(), windows.replace('\n', "\r\n")] {
    let text = rights_refusal(&output, data).expect("the refusal is recognised");
    for needle in [
      "PostgreSQL did not start.",
      "does not run with administrator rights",
      "Run as administrator",
      "start it without administrator rights",
      "The library in /d a é/data is kept as it is.",
    ] {
      assert!(text.contains(needle), "{needle}: {text}");
    }
  }
  let unix = "\"root\" execution of the PostgreSQL server is not permitted.\n\
              The server must be started under an unprivileged user ID to prevent\n\
              possible system security compromise.  See the documentation for\n\
              more information on how to properly start the server.\n";
  let text = rights_refusal(unix, data).expect("the refusal is recognised");
  assert!(
    text.contains("does not run as root") && text.contains("as an ordinary user"),
    "{text}"
  );
  for other in [
    "",
    "2026-10-12 09:00:00.000 UTC FATAL:  lock file \"postmaster.pid\" is empty",
    "2026-10-12 09:00:00.000 UTC FATAL:  data directory \"C:/d\" has invalid permissions",
    "2026-10-12 09:00:00.000 UTC LOG:  could not bind IPv4 address \"127.0.0.1\": Address already in use",
  ] {
    assert_eq!(rights_refusal(other, data), None, "{other}");
  }
}

/// The new cluster folder is renamed into place when the other programs have let its
/// files go: on Windows a refusal for lack of permission is tried again, for a bounded
/// time. The refusal is made here with a parent folder that may not be written to, which
/// gives the same kind of error on Unix.
#[cfg(unix)]
#[test]
fn the_rename_of_a_new_cluster_waits_for_a_refusal_to_pass_on_windows_only() {
  use std::os::unix::fs::PermissionsExt;
  // SAFETY: no arguments, no side effect. Root may write everywhere: nothing to refuse.
  if unsafe { libc::geteuid() } == 0 {
    return;
  }
  let root = Scratch::new();
  let parent = root.0.join("postgres 18");
  let (from, to) = (parent.join("data.init-1"), parent.join("data"));
  fs::create_dir_all(&from).unwrap();
  fs::write(from.join("PG_VERSION"), "18\n").unwrap();
  let lock = |locked: bool| {
    let mode = if locked { 0o500 } else { 0o700 };
    fs::set_permissions(&parent, fs::Permissions::from_mode(mode)).unwrap();
  };
  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
  let long = Duration::from_secs(60);
  let denied = std::io::ErrorKind::PermissionDenied;

  // Not Windows: one try, the error at once, whatever the wait.
  lock(true);
  let before = Instant::now();
  let error = runtime
    .block_on(rename_when_free(&from, &to, false, long))
    .unwrap_err();
  assert_eq!(error.kind(), denied);
  assert!(before.elapsed() < Duration::from_secs(10));

  // Windows, and the refusal does not pass: the same error, after the wait.
  let before = Instant::now();
  let wait = Duration::from_millis(400);
  let error = runtime
    .block_on(rename_when_free(&from, &to, true, wait))
    .unwrap_err();
  assert_eq!(error.kind(), denied);
  assert!(before.elapsed() >= wait, "{:?}", before.elapsed());
  assert!(from.exists() && !to.exists());

  // Windows, and the refusal passes after a moment: renamed.
  let release = {
    let parent = parent.clone();
    std::thread::spawn(move || {
      std::thread::sleep(Duration::from_millis(400));
      fs::set_permissions(&parent, fs::Permissions::from_mode(0o700)).unwrap();
    })
  };
  runtime
    .block_on(rename_when_free(&from, &to, true, long))
    .expect("renamed once the refusal passed");
  release.join().unwrap();
  assert_eq!(fs::read_to_string(to.join("PG_VERSION")).unwrap(), "18\n");
  assert!(!from.exists());

  // Windows, another error (nothing to rename): at once, not after the wait.
  let before = Instant::now();
  let error = runtime
    .block_on(rename_when_free(&from, &to, true, long))
    .unwrap_err();
  assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
  assert!(before.elapsed() < Duration::from_secs(10));
  lock(false);
}
