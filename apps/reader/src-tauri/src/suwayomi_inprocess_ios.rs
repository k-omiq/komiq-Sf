//! iOS half of the in-process engine (`suwayomi_inprocess.rs`): where the JVM and its
//! files come from on a REAL iOS device (N4.2).
//!
//! iOS forbids app-loaded dylibs, so the JVM is the interpreter-only (Zero, NO-JIT)
//! OpenJDK that N4.1 built as static libs (`jre-ios/aarch64-ios`, statically linked into
//! the app binary — see `build.rs`). `JNI_CreateJavaVM` is an ordinary symbol of the app.
//!
//! Runtime layout (why the app bundle carries a root-level `lib/` folder): the
//! statically-linked HotSpot derives `java.home` ITSELF as `<exe dir>/lib` — on
//! iOS `os::init_system_properties_values()` takes the dladdr path of the app
//! executable, strips the binary name, and appends `/lib` because
//! `is_vm_statically_linked()` is true (verified against the pinned
//! openjdk/mobile sources, os_bsd.cpp `__IOS__` branch). `-Djava.home` CANNOT
//! relocate the boot jimage: `set_boot_path()` (=> `<java.home>/lib/modules`)
//! runs during property init, BEFORE `-D` options are applied. The bundle must
//! therefore contain:
//!   Komika.app/lib/lib/modules            <- jimage (all 66 modules, ~149 MB)
//!   Komika.app/lib/lib/{tzdb.dat,security,jfr,jvm.cfg}
//!   Komika.app/lib/{conf,release}
//! staged into the Xcode project by scripts/stage-ios-jvm-runtime.sh (a folder
//! reference in gen/apple/project.yml). The jar itself IS a normal tauri
//! resource (`tauri.ios.conf.json` bundles `suwayomi/`, resolved via
//! `resource_dir()` => Komika.app/assets/suwayomi/Suwayomi-Server.jar).

use std::ffi::c_void;
use std::path::PathBuf;

use jni_sys::{jint, JavaVM, JNI_CreateJavaVM};
use tauri::{AppHandle, Manager};

/// `-Xss` for every thread the VM creates ITSELF (Javalin/OkHttp workers, the
/// extension installer's class rewriting, the class loaders) — the threads that
/// actually run deep recursive Java code. Same reasoning as the VM thread's own stack:
/// under Zero every Java frame costs a C frame of the bytecode interpreter, so the
/// HotSpot default (~1 MiB) buys a fraction of the Java depth it buys on a JIT VM,
/// and a StackOverflowError here shows up as an engine that is `ready` but never
/// serves a chapter. Thread stacks are reserved address space committed page by
/// page, so the RSS cost is the depth actually used; the peak thread count is an
/// N4.3 measurement input. Desktop's JIT sidecar sets no `-Xss` at all.
const JAVA_THREAD_STACK_OPT: &str = "-Xss4m";
/// The `java.util.prefs` backend, set explicitly because the default of this runtime
/// cannot work. `Preferences` picks its backend from the OS the classes were built for:
/// macOS gets `MacOSXPreferencesFactory`, every other non-Windows OS (iOS included) gets
/// `FileSystemPreferencesFactory`. But the iOS build compiles `libprefs.a` from the
/// macOS native sources, so the app holds the natives of the macOS backend only and
/// `FileSystemPreferences.{chmod,lockFile0,unlockFile0}` do not exist (checked with `nm`
/// on the app binary). The result on a phone (2026-10-05): Suwayomi's first migration
/// calls `Preferences.userRoot()`, the file backend calls `chmod` on each of the two
/// files it creates, and the `UnsatisfiedLinkError` ended `main` on the first two
/// launches of a new install; after that its sync timer failed in `lockFile0` every
/// 30 s. Naming the macOS backend uses the natives that are linked, and is the same
/// code path the macOS desktop engine runs.
const PREFS_FACTORY_OPT: &str =
  "-Djava.util.prefs.PreferencesFactory=java.util.prefs.MacOSXPreferencesFactory";
/// The options only this system's VM gets, after the shared ones.
pub(super) const JVM_OPTIONS: &[&str] = &[JAVA_THREAD_STACK_OPT, PREFS_FACTORY_OPT];

/// Check the runtime in the app bundle and give the engine's class path (the jar).
pub(super) fn prepare(app: &AppHandle) -> Result<String, String> {
  check_bundled_runtime()?;
  Ok(resolve_jar(app)?.display().to_string())
}

/// `JNI_CreateJavaVM` of the statically linked VM.
///
/// # Safety
/// The three pointers are the arguments of `JNI_CreateJavaVM` and must be valid for it.
pub(super) unsafe fn create_java_vm(
  vm: *mut *mut JavaVM,
  env: *mut *mut c_void,
  args: *mut c_void,
) -> Result<jint, String> {
  Ok(JNI_CreateJavaVM(vm, env, args))
}

/// Resolve the bundled jar via tauri's resource resolver
/// (=> Komika.app/assets/suwayomi/Suwayomi-Server.jar on iOS).
fn resolve_jar(app: &AppHandle) -> Result<PathBuf, String> {
  let res = app
    .path()
    .resource_dir()
    .map_err(|e| format!("resolve resource_dir: {e}"))?;
  let cand = res.join("suwayomi").join("Suwayomi-Server.jar");
  if cand.exists() {
    Ok(cand)
  } else {
    Err(format!(
      "Suwayomi-Server.jar not bundled at {} (tauri.ios.conf.json resources)",
      cand.display()
    ))
  }
}

/// Sanity-check the JVM runtime the app bundle must carry at <exe dir>/lib (the
/// java.home the statically-linked VM derives — see module docs). Catching this
/// here yields a clean `degraded` + actionable error instead of a VM abort.
fn check_bundled_runtime() -> Result<(), String> {
  let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
  let app_dir = exe
    .parent()
    .ok_or_else(|| "executable has no parent dir".to_string())?;
  let jimage = app_dir.join("lib").join("lib").join("modules");
  if jimage.exists() {
    Ok(())
  } else {
    Err(format!(
      "bundled JVM runtime missing: {} not found (stage it with \
       scripts/stage-ios-jvm-runtime.sh and rebuild)",
      jimage.display()
    ))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The Zero interpreter needs more Java thread stack than the default.
  #[test]
  fn jvm_options_size_the_java_thread_stacks() {
    assert!(JVM_OPTIONS.contains(&"-Xss4m"), "options: {JVM_OPTIONS:?}");
  }

  /// The runtime's default prefs backend has no natives in the app (see
  /// `PREFS_FACTORY_OPT`), so the VM must always be told to use the macOS one.
  #[test]
  fn jvm_options_name_the_prefs_backend_that_is_linked() {
    assert!(
      JVM_OPTIONS
        .contains(&"-Djava.util.prefs.PreferencesFactory=java.util.prefs.MacOSXPreferencesFactory"),
      "options: {JVM_OPTIONS:?}"
    );
  }
}
