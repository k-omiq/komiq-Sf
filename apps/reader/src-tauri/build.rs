use std::path::PathBuf;

/// N4.2: link the interpreter-only (Zero) OpenJDK produced by N4.1 into the iOS
/// DEVICE build only (`target_os = "ios"` and NOT the simulator — the vendored
/// `.a` files are device-slice only, so the sim must keep building the engine
/// stub with no JVM link flags).
///
/// Split of responsibilities (this crate builds as a `staticlib` for iOS; the
/// FINAL link is done by Xcode via gen/apple):
///
///   * build.rs (here): bundles `libjvm.a` (the Zero VM — `JNI_CreateJavaVM` is
///     referenced directly from `suwayomi_ios.rs`, so ld pulls the VM's members
///     transitively) and `libffi.a` (Zero calls native functions through libffi;
///     its symbols are referenced by the VM's objects) into the emitted
///     `libapp.a`. rustc's default `+bundle` semantics for `static=` libs makes
///     both travel inside the staticlib.
///
///   * gen/apple/project.yml (hand-maintained, gitignored; see
///     scripts/stage-ios-jvm-runtime.sh): `-Wl,-force_load` for the sibling JDK
///     static libs (libjava, libnio, libnet, libzip, libjimage, libverify,
///     libprefs, libextnet, libsyslookup, libfallbackLinker, libmanagement{,_ext},
///     librmi) — those CANNOT be bundled here: nothing references their
///     `Java_*`/`JNI_OnLoad_*` members at link time (HotSpot resolves them via
///     dlsym on the app image at runtime, `os::dll_load` returns the process
///     handle when `is_vm_statically_linked()`), so as ordinary archive members
///     they would be dropped; and force-loading the whole `libapp.a` instead is
///     impossible because tauri's staticlib legitimately carries duplicate Swift
///     runtime objects that only normal archive semantics tolerate. project.yml
///     also carries the system deps `-lz -liconv -lc++` +
///     CoreFoundation/Foundation frameworks (from an undefined-symbol audit of
///     the vendored .a set) and DEAD_CODE_STRIPPING=false (a stripped
///     force_load re-drops the dlsym-only members).
///
///   * Deliberately NOT linked at all (present in jre-ios/aarch64-ios/lib,
///     unused by the Suwayomi boot path; add to project.yml if the engine ever
///     throws `UnsatisfiedLinkError` naming one): jsig (preload-style signal
///     interposer), jli (launcher machinery; we invoke JNI directly), jdwp,
///     dt_socket, attach, instrument, management_agent (debug/agent infra),
///     j2pkcs11, j2gss, j2pcsc (smartcard/kerberos providers).
///
/// If the N4.1 artifact is absent this prints a warning and emits nothing:
/// desktop and sim builds are unaffected, and an iOS DEVICE build fails at link
/// with undefined `JNI_CreateJavaVM` — run scripts/build-ios-jvm.sh first
/// (expected, documented; the artifact is never committed).
fn ios_device_jvm_link() {
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let target_abi = std::env::var("CARGO_CFG_TARGET_ABI").unwrap_or_default();
  if target_os != "ios" || target_abi == "sim" {
    return;
  }

  let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
  let jre = manifest_dir.join("jre-ios").join("aarch64-ios");
  let libjvm = jre.join("lib").join("zero").join("libjvm.a");
  if !libjvm.exists() {
    println!(
      "cargo:warning=jre-ios/aarch64-ios/lib/zero/libjvm.a not found — the iOS device \
       build will fail at link (undefined JNI_CreateJavaVM). Build the N4.1 runtime \
       first: apps/reader/src-tauri/scripts/build-ios-jvm.sh"
    );
    return;
  }
  let libffi = jre.join("lib").join("support").join("libffi.a");
  if !libffi.exists() {
    println!(
      "cargo:warning=jre-ios/aarch64-ios/lib/support/libffi.a not found — stage the \
       Gluon iOS libffi.a there (see scripts/build-ios-jvm.sh SUPPORT_URL); the Zero \
       interpreter cannot call native code without it"
    );
  }

  println!("cargo:rerun-if-changed={}", libjvm.display());
  for dir in ["lib/zero", "lib", "lib/support"] {
    println!("cargo:rustc-link-search=native={}", jre.join(dir).display());
  }
  // The Zero interpreter VM (JNI_CreateJavaVM lives here) and the libffi it uses
  // to call native functions (Zero has no template interpreter).
  println!("cargo:rustc-link-lib=static=jvm");
  println!("cargo:rustc-link-lib=static=ffi");
  // W^X link shim: the pinned zero archive has two dangling MACOS_AARCH64
  // references whose definers only exist in the (uncompiled) bsd_aarch64 port —
  // see scripts/ios-jvm-wx-shim.cpp for the full analysis.
  build_wx_shim(&manifest_dir);
  // The VM's system deps. For the staticlib artifact these are only recorded
  // (Xcode's OTHER_LDFLAGS/frameworks must repeat them for the app link), but the
  // crate ALSO builds a cdylib for iOS and rustc fully links that one — without
  // these its libjvm members' undefined _deflate/_iconv/CF/C++ symbols fail the
  // build before Xcode ever runs.
  println!("cargo:rustc-link-lib=z");
  println!("cargo:rustc-link-lib=iconv");
  println!("cargo:rustc-link-lib=c++");
  println!("cargo:rustc-link-lib=framework=CoreFoundation");
  println!("cargo:rustc-link-lib=framework=Foundation");
}

/// Compile scripts/ios-jvm-wx-shim.cpp for the iOS device target and link it as a
/// static lib. Panics on failure: without the shim the device link is guaranteed
/// to fail with the same two undefined symbols, only less legibly.
fn build_wx_shim(manifest_dir: &std::path::Path) {
  let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
  let src = manifest_dir.join("scripts").join("ios-jvm-wx-shim.cpp");
  let obj = out_dir.join("ios_jvm_wx_shim.o");
  let lib = out_dir.join("libios_jvm_wx_shim.a");
  println!("cargo:rerun-if-changed={}", src.display());

  let clang = std::process::Command::new("xcrun")
    .args(["--sdk", "iphoneos", "clang++"])
    .args(["-target", "arm64-apple-ios14.0", "-O2", "-c"])
    .arg(&src)
    .arg("-o")
    .arg(&obj)
    .status()
    .expect("run xcrun clang++ for ios-jvm-wx-shim");
  assert!(clang.success(), "compile ios-jvm-wx-shim.cpp failed");

  let _ = std::fs::remove_file(&lib);
  let ar = std::process::Command::new("xcrun")
    .args(["--sdk", "iphoneos", "ar", "crs"])
    .arg(&lib)
    .arg(&obj)
    .status()
    .expect("run xcrun ar for ios-jvm-wx-shim");
  assert!(ar.success(), "archive ios-jvm-wx-shim failed");

  println!("cargo:rustc-link-search=native={}", out_dir.display());
  println!("cargo:rustc-link-lib=static=ios_jvm_wx_shim");
}

fn main() {
  ios_device_jvm_link();
  tauri_build::build()
}
