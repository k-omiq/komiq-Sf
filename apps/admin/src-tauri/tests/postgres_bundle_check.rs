#[path = "../postgres_bundle_check.rs"]
mod check;
use serde_json::json;

#[test]
fn release_payload_missing_empty_wrong_target_and_right_target() {
  let root = std::env::temp_dir().join(format!("komiq-build-payload-{}", std::process::id()));
  std::fs::create_dir(&root).unwrap();
  let triple = "aarch64-apple-darwin";
  assert!(check::check(&root, triple).is_err());
  let tree = root.join("aarch64-macos/18");
  std::fs::create_dir_all(&tree).unwrap();
  std::fs::write(tree.join("manifest.json"), "{}").unwrap();
  assert!(check::check(&root, triple).is_err());
  let mut files = serde_json::Map::new();
  for name in [
    "bin/postgres",
    "bin/initdb",
    "bin/pg_ctl",
    "bin/pg_dump",
    "bin/pg_restore",
    "bin/psql",
    "lib/pg_trgm.dylib",
    "share/postgres.bki",
    "share/postgresql.conf.sample",
    "share/pg_hba.conf.sample",
    "share/pg_ident.conf.sample",
    "share/information_schema.sql",
    "share/system_views.sql",
    "share/system_functions.sql",
    "share/system_constraints.sql",
    "share/sql_features.txt",
    "share/snowball_create.sql",
    "share/extension/pg_trgm.control",
    "share/extension/pg_trgm--1.6.sql",
    "share/timezonesets/Default",
    "share/tsearch_data/english.stop",
  ] {
    let path = tree.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, "fixture").unwrap();
    files.insert(name.into(), json!("fixture hash"));
  }
  let mut manifest = json!({"version":"18.6","target":"aarch64-macos","triple":triple,
    "required":files.keys().collect::<Vec<_>>(),"files":files});
  let path = tree.join("manifest.json");
  std::fs::write(&path, manifest.to_string()).unwrap();
  let tracked = check::check(&root, triple).unwrap();
  assert!(tracked.contains(&tree.join("share/postgres.bki")));
  assert!(check::check(&root, "x86_64-apple-darwin").is_err());
  manifest["target"] = json!("x86_64-macos");
  std::fs::write(&path, manifest.to_string()).unwrap();
  assert!(check::check(&root, triple).is_err());
  manifest["target"] = json!("aarch64-macos");
  manifest["version"] = json!("17.6");
  std::fs::write(&path, manifest.to_string()).unwrap();
  assert!(check::check(&root, triple).is_err());
  manifest["version"] = json!("18.6");
  std::fs::write(&path, manifest.to_string()).unwrap();
  std::fs::write(tree.join("bin/postgres"), "").unwrap();
  assert!(check::check(&root, triple).is_err());
  std::fs::write(tree.join("bin/postgres"), "fixture").unwrap();
  std::fs::remove_file(tree.join("share/postgres.bki")).unwrap();
  assert!(check::check(&root, triple).is_err());
  std::fs::remove_dir_all(root).unwrap();
}

/// The payload staged for this build target (`scripts/postgres-desktop-bundle.py
/// --triple <triple>`) through the same check a release build runs in build.rs.
/// `KOMIKA_POSTGRES_BUNDLE_TRIPLE` names the target; run with `--ignored`.
#[test]
#[ignore = "needs the staged PostgreSQL payload of this target"]
fn staged_payload_of_this_target_passes_the_release_check() {
  let triple = std::env::var("KOMIKA_POSTGRES_BUNDLE_TRIPLE")
    .expect("KOMIKA_POSTGRES_BUNDLE_TRIPLE names the build target");
  let bundle = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("postgres/bundle");
  let tracked = check::check(&bundle, &triple)
    .unwrap_or_else(|error| panic!("staged payload refused for {triple}: {error}"));
  let slug = check::slug(&triple).unwrap();
  let exe = if slug.ends_with("windows") {
    ".exe"
  } else {
    ""
  };
  for tool in [
    "postgres",
    "initdb",
    "pg_ctl",
    "pg_dump",
    "pg_restore",
    "psql",
  ] {
    let path = bundle
      .join(slug)
      .join("18/bin")
      .join(format!("{tool}{exe}"));
    assert!(tracked.contains(&path), "{} is tracked", path.display());
    assert!(std::fs::metadata(&path).unwrap().len() > 0);
  }
  // The same tree is refused for every other target.
  for other in [
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-pc-windows-msvc",
  ] {
    if other != triple {
      assert!(check::check(&bundle, other).is_err(), "{other}");
    }
  }
}

/// Each target the payload can be staged for has a folder name, and it is the name the
/// running app looks for (`postgres::target_slug`: `<ARCH>-<OS>` of the Rust target).
/// The same table is in scripts/postgres-desktop-bundle.py (`TARGETS`).
#[test]
fn every_target_has_its_payload_folder_name() {
  for (triple, folder) in [
    ("aarch64-apple-darwin", "aarch64-macos"),
    ("x86_64-apple-darwin", "x86_64-macos"),
    ("x86_64-unknown-linux-gnu", "x86_64-linux"),
    ("aarch64-unknown-linux-gnu", "aarch64-linux"),
    ("x86_64-pc-windows-msvc", "x86_64-windows"),
  ] {
    assert_eq!(check::slug(triple).unwrap(), folder);
    let mut parts = triple.split('-');
    let arch = parts.next().unwrap();
    let os = match triple {
      t if t.ends_with("apple-darwin") => "macos",
      t if t.ends_with("linux-gnu") => "linux",
      _ => "windows",
    };
    assert_eq!(folder, format!("{arch}-{os}"));
  }
  assert!(check::slug("aarch64-pc-windows-msvc").is_err());
}
