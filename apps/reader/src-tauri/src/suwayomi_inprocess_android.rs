//! Android half of the in-process engine (`suwayomi_inprocess.rs`): where the JVM and its
//! files come from on Android.
//!
//! The JVM is HotSpot from Termux's `openjdk-21` package (JIT, built for bionic), made
//! ready by `scripts/build-android-jvm.sh`. An APK built without that step has no engine,
//! and `runtime_bundled` says so. The parts and where they are at run time:
//!
//!   <native library dir>/libjvm.so, libjava.so, …   the app's own native libraries,
//!                                                   installed read-only by Android
//!   <app data>/engine/runtime/                      java.home: lib/modules, tzdb, conf
//!   <app data>/engine/zstd-android.jar              zstd for Android (class path, first)
//!   <app data>/engine/Suwayomi-Server.jar           the engine
//!
//! The `engine/` files travel in the APK as assets and are unpacked once per install
//! (`unpack_engine`), because a JVM needs real files.
//!
//! Three things HotSpot needs that an app does not give it by itself:
//!   * java.home. This HotSpot takes it from the path of `libjvm.so`
//!     (`<java.home>/lib/server/libjvm.so`), but the library is in the native library
//!     dir. `scripts/android-jvm-shim.c` (packed as `libandroid-spawn.so`) answers
//!     HotSpot's `dladdr` with the path it expects, under the java.home given to
//!     `komiq_jvm_set_home`.
//!   * Its own libraries in `<java.home>/lib`. Some are loaded before any option is
//!     read, so no option can name another place. `link_libraries` puts a link to each
//!     one there.
//!   * Untagged heap pointers. Android tags them in the top byte and HotSpot drops the
//!     tag, which ends the process. The manifest sets
//!     `android:allowNativeHeapPointerTagging="false"`; `prepare` refuses to start the
//!     VM if that is missing.

use std::ffi::{c_char, c_void, CStr, CString};
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use jni_sys::{jint, JavaVM};
use tauri::{AppHandle, Manager};

/// bionic has `posix_spawn` from API 28 (Android 9); the stand-in for
/// `libandroid-spawn.so` relies on it.
const MIN_API_LEVEL: u32 = 28;
/// Where the engine's files are inside the APK.
const APK_ENGINE_DIR: &str = "assets/engine/";

/// The options only this system's VM gets, after the shared ones.
///   * `-Xrs`: HotSpot leaves SIGQUIT, SIGTERM, SIGINT and SIGHUP alone. They belong to
///     the Android runtime in this process (SIGQUIT is how it writes ANR traces).
///   * `-XX:-UsePerfData`: no `hsperfdata` file; nothing on a phone reads it.
pub(super) const JVM_OPTIONS: &[&str] = &["-Xrs", "-XX:-UsePerfData"];

/// The directory Android installed this app's native libraries in: the one this very
/// library was loaded from.
fn native_library_dir() -> Result<PathBuf, String> {
  let mut info: libc::Dl_info = unsafe { std::mem::zeroed() };
  let found = unsafe { libc::dladdr(native_library_dir as *const c_void, &mut info) };
  if found == 0 || info.dli_fname.is_null() {
    return Err("cannot find the app's native library dir".to_string());
  }
  let this_library = PathBuf::from(
    unsafe { CStr::from_ptr(info.dli_fname) }
      .to_string_lossy()
      .into_owned(),
  );
  this_library
    .parent()
    .map(Path::to_path_buf)
    .ok_or_else(|| "cannot find the app's native library dir".to_string())
}

fn api_level() -> u32 {
  // PROP_VALUE_MAX is 92.
  let mut value = [0 as c_char; 92];
  let len =
    unsafe { libc::__system_property_get(c"ro.build.version.sdk".as_ptr(), value.as_mut_ptr()) };
  if len <= 0 {
    return 0;
  }
  unsafe { CStr::from_ptr(value.as_ptr()) }
    .to_str()
    .ok()
    .and_then(|v| v.trim().parse().ok())
    .unwrap_or(0)
}

/// Does this build carry the engine, and can this phone run it? The reader offers
/// "This device" only when this is true.
pub fn runtime_bundled() -> bool {
  api_level() >= MIN_API_LEVEL
    && native_library_dir().is_ok_and(|dir| dir.join("libjvm.so").is_file())
}

/// Does `malloc` give tagged pointers (a tag in the top byte)?
fn heap_pointers_are_tagged() -> bool {
  unsafe {
    let probe = libc::malloc(16);
    let tagged = (probe as usize) >> 56 != 0;
    libc::free(probe);
    tagged
  }
}

/// Copy the engine's files out of the APK into `engine_dir`, unless that was already
/// done from this same APK. The copy is made beside the target and moved into place, so
/// a start that is cut short leaves the old state or none, never half of the files.
fn unpack_engine(apk: &Path, engine_dir: &Path) -> Result<(), String> {
  let meta = fs::metadata(apk).map_err(|e| format!("read {}: {e}", apk.display()))?;
  let modified = meta
    .modified()
    .ok()
    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
    .map_or(0, |d| d.as_secs());
  // An update installs the APK at a new path, so this changes with every install.
  let source = format!("{}\n{}\n{modified}\n", apk.display(), meta.len());
  let marker = engine_dir.join(".source");
  if fs::read_to_string(&marker).is_ok_and(|seen| seen == source) {
    return Ok(());
  }

  let started = Instant::now();
  let staging = engine_dir.with_extension("tmp");
  let _ = fs::remove_dir_all(&staging);
  let file = fs::File::open(apk).map_err(|e| format!("open {}: {e}", apk.display()))?;
  let mut archive =
    zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| format!("read the APK: {e}"))?;
  let mut files = 0u32;
  for index in 0..archive.len() {
    let mut entry = archive
      .by_index(index)
      .map_err(|e| format!("read the APK: {e}"))?;
    let Some(inside) = entry.enclosed_name() else {
      continue;
    };
    let Ok(relative) = inside.strip_prefix(APK_ENGINE_DIR) else {
      continue;
    };
    if entry.is_dir() || relative.as_os_str().is_empty() {
      continue;
    }
    let target = staging.join(relative);
    if let Some(parent) = target.parent() {
      fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let mut out =
      fs::File::create(&target).map_err(|e| format!("create {}: {e}", target.display()))?;
    io::copy(&mut entry, &mut out).map_err(|e| format!("unpack {}: {e}", target.display()))?;
    files += 1;
  }
  if files == 0 {
    return Err(format!(
      "this build has no engine files ({APK_ENGINE_DIR} in the APK is empty); run \
       scripts/build-android-jvm.sh and build again"
    ));
  }
  fs::write(staging.join(".source"), &source).map_err(|e| format!("write the unpack marker: {e}"))?;
  let _ = fs::remove_dir_all(engine_dir);
  fs::rename(&staging, engine_dir).map_err(|e| format!("move the engine into place: {e}"))?;
  log::info!(
    target: "suwayomi",
    "unpacked {files} engine files from the APK in {} ms",
    started.elapsed().as_millis()
  );
  Ok(())
}

/// Put a link to every native library of the app into `<java.home>/lib`, where HotSpot
/// looks for its own. The native library dir gets a new path with every install, so the
/// links are made again on every start.
fn link_libraries(library_dir: &Path, java_lib_dir: &Path) -> Result<(), String> {
  let entries =
    fs::read_dir(library_dir).map_err(|e| format!("read {}: {e}", library_dir.display()))?;
  for entry in entries.flatten() {
    let name = entry.file_name();
    if !name.as_bytes().starts_with(b"lib") || !name.as_bytes().ends_with(b".so") {
      continue;
    }
    let link = java_lib_dir.join(&name);
    let _ = fs::remove_file(&link);
    std::os::unix::fs::symlink(entry.path(), &link)
      .map_err(|e| format!("link {}: {e}", link.display()))?;
  }
  Ok(())
}

fn c_path(path: &Path) -> Result<CString, String> {
  CString::new(path.as_os_str().as_bytes()).map_err(|_| format!("NUL in {}", path.display()))
}

/// `dlopen` one of the app's own libraries by its full path.
fn open_library(path: &Path) -> Result<*mut c_void, String> {
  let handle = unsafe { libc::dlopen(c_path(path)?.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
  if handle.is_null() {
    let reason = unsafe {
      let message = libc::dlerror();
      if message.is_null() {
        "unknown error".to_string()
      } else {
        CStr::from_ptr(message).to_string_lossy().into_owned()
      }
    };
    return Err(format!("dlopen {}: {reason}", path.display()));
  }
  Ok(handle)
}

fn symbol(library: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
  let address = unsafe { libc::dlsym(library, name.as_ptr()) };
  if address.is_null() {
    return Err(format!("{} not found in the JVM libraries", name.to_string_lossy()));
  }
  Ok(address)
}

/// Make the runtime ready (unpack, links, java.home) and give the engine's class path.
/// Everything here runs before the VM exists, so a failure is an ordinary error.
pub(super) fn prepare(app: &AppHandle) -> Result<String, String> {
  let api = api_level();
  if api < MIN_API_LEVEL {
    return Err(format!(
      "the on-device engine needs Android 9 (API {MIN_API_LEVEL}) or newer; this is API {api}"
    ));
  }
  if heap_pointers_are_tagged() {
    return Err(
      "the app manifest must set android:allowNativeHeapPointerTagging=\"false\" for the \
       on-device engine"
        .to_string(),
    );
  }
  let library_dir = native_library_dir()?;
  if !library_dir.join("libjvm.so").is_file() {
    return Err(format!(
      "this build has no engine runtime (no libjvm.so in {}); run \
       scripts/build-android-jvm.sh and build again",
      library_dir.display()
    ));
  }
  // <code dir>/lib/<abi> -> <code dir>/base.apk
  let apk = library_dir
    .parent()
    .and_then(Path::parent)
    .map(|code_dir| code_dir.join("base.apk"))
    .ok_or_else(|| format!("no APK beside {}", library_dir.display()))?;
  let engine_dir = app
    .path()
    .app_data_dir()
    .map_err(|e| format!("resolve app_data_dir: {e}"))?
    .join("engine");
  unpack_engine(&apk, &engine_dir)?;

  let java_home = engine_dir.join("runtime");
  link_libraries(&library_dir, &java_home.join("lib"))?;
  let shim = open_library(&library_dir.join("libandroid-spawn.so"))?;
  let set_home: unsafe extern "C" fn(*const c_char) =
    unsafe { std::mem::transmute(symbol(shim, c"komiq_jvm_set_home")?) };
  unsafe { set_home(c_path(&java_home)?.as_ptr()) };

  Ok(format!(
    "{}:{}",
    engine_dir.join("zstd-android.jar").display(),
    engine_dir.join("Suwayomi-Server.jar").display()
  ))
}

/// Load `libjvm.so` and call its `JNI_CreateJavaVM`. `prepare` must have run.
///
/// # Safety
/// The three pointers are the arguments of `JNI_CreateJavaVM` and must be valid for it.
pub(super) unsafe fn create_java_vm(
  vm: *mut *mut JavaVM,
  env: *mut *mut c_void,
  args: *mut c_void,
) -> Result<jint, String> {
  let jvm = open_library(&native_library_dir()?.join("libjvm.so"))?;
  let create: unsafe extern "system" fn(*mut *mut JavaVM, *mut *mut c_void, *mut c_void) -> jint =
    std::mem::transmute(symbol(jvm, c"JNI_CreateJavaVM")?);
  Ok(create(vm, env, args))
}
