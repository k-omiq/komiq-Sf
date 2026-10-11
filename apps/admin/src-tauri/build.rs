mod postgres_bundle_check;

fn main() {
  println!("cargo:rerun-if-env-changed=PROFILE");
  println!("cargo:rerun-if-env-changed=TARGET");
  println!("cargo:rerun-if-env-changed=TAURI_ENV_DEBUG");
  println!("cargo:rerun-if-changed=postgres/bundle");
  println!("cargo:rerun-if-changed=build.rs");
  println!("cargo:rerun-if-changed=postgres_bundle_check.rs");
  if std::env::var("PROFILE").as_deref() != Ok("debug")
    || std::env::var("TAURI_ENV_DEBUG").as_deref() == Ok("false")
  {
    let triple = std::env::var("TARGET").expect("Cargo build target");
    let files = postgres_bundle_check::check(std::path::Path::new("postgres/bundle"), &triple)
      .unwrap_or_else(|error| panic!("PostgreSQL release payload invalid: {error}. Stage this target with scripts/postgres-desktop-bundle.py --triple {triple} before building/bundling."));
    for file in files {
      println!("cargo:rerun-if-changed={}", file.display());
    }
  } else {
    // Offline debug check/test retains its resource placeholder. Bundle hooks
    // independently validate even `tauri bundle --debug` without recompilation.
    std::fs::create_dir_all("postgres/bundle").expect("create debug resource placeholder");
  }
  tauri_build::build()
}
