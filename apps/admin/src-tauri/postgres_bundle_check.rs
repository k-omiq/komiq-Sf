//! Offline release-resource validation, shared by build.rs and its integration test.
use std::path::{Component, Path, PathBuf};

pub fn slug(triple: &str) -> Result<&'static str, String> {
  match triple {
    "aarch64-apple-darwin" => Ok("aarch64-macos"),
    "x86_64-apple-darwin" => Ok("x86_64-macos"),
    "x86_64-unknown-linux-gnu" => Ok("x86_64-linux"),
    "aarch64-unknown-linux-gnu" => Ok("aarch64-linux"),
    "x86_64-pc-windows-msvc" => Ok("x86_64-windows"),
    _ => Err(format!("unsupported PostgreSQL target: {triple}")),
  }
}

pub fn check(bundle: &Path, triple: &str) -> Result<Vec<PathBuf>, String> {
  let target = slug(triple)?;
  let root = bundle.join(target).join("18");
  let manifest_path = root.join("manifest.json");
  let manifest: serde_json::Value = serde_json::from_slice(
    &std::fs::read(&manifest_path).map_err(|e| format!("{}: {e}", manifest_path.display()))?,
  )
  .map_err(|e| e.to_string())?;
  if manifest["version"] != "18.6" || manifest["target"] != target || manifest["triple"] != triple {
    return Err(format!(
      "manifest must pin PostgreSQL 18.6 for {triple} ({target})"
    ));
  }
  for entry in std::fs::read_dir(bundle).map_err(|e| e.to_string())? {
    if entry.map_err(|e| e.to_string())?.file_name() != target {
      return Err("bundle contains another target or unowned entry".into());
    }
  }
  let files = manifest["files"]
    .as_object()
    .filter(|f| !f.is_empty())
    .ok_or("empty manifest files")?;
  let required = manifest["required"]
    .as_array()
    .filter(|r| !r.is_empty())
    .ok_or("empty required list")?;
  let exe = if target.ends_with("windows") {
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
    if !files.contains_key(&format!("bin/{tool}{exe}")) {
      return Err(format!("missing program {tool}"));
    }
  }
  for share in [
    "postgres.bki",
    "postgresql.conf.sample",
    "pg_hba.conf.sample",
    "pg_ident.conf.sample",
    "information_schema.sql",
    "system_views.sql",
    "system_functions.sql",
    "system_constraints.sql",
    "sql_features.txt",
    "snowball_create.sql",
    "extension/pg_trgm.control",
  ] {
    if !files.contains_key(&format!("share/{share}")) {
      return Err(format!("missing share/{share}"));
    }
  }
  for prefix in [
    "share/extension/pg_trgm--",
    "share/timezonesets/",
    "share/tsearch_data/",
  ] {
    if !files.keys().any(|n| n.starts_with(prefix)) {
      return Err(format!("missing {prefix}"));
    }
  }
  if !files
    .keys()
    .any(|n| n.starts_with("lib/") && n.contains("pg_trgm."))
  {
    return Err("missing pg_trgm library".into());
  }
  let canonical = root.canonicalize().map_err(|e| e.to_string())?;
  let mut tracked = vec![bundle.to_path_buf(), root.clone(), manifest_path];
  for name in files
    .keys()
    .map(String::as_str)
    .chain(required.iter().map(|v| v.as_str().unwrap_or("")))
  {
    let path = Path::new(name);
    if name.is_empty()
      || name.contains('\\')
      || path
        .components()
        .any(|c| !matches!(c, Component::Normal(_)))
    {
      return Err(format!("unsafe manifest path: {name}"));
    }
    let resource = root.join(path);
    let resolved = resource
      .canonicalize()
      .map_err(|e| format!("{name}: {e}"))?;
    let meta = std::fs::metadata(&resolved).map_err(|e| e.to_string())?;
    if !resolved.starts_with(&canonical) || !meta.is_file() || meta.len() == 0 {
      return Err(format!("missing, empty or escaping resource: {name}"));
    }
    tracked.push(resource);
  }
  Ok(tracked)
}
