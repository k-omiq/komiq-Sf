// N4.2 link shim for the iOS Zero JVM static archive (jre-ios/aarch64-ios).
//
// UPSTREAM GAP (openjdk/mobile @ 7e8729ef9298): shared HotSpot code compiled with
// MACOS_AARCH64 (heap.o, codeBlob.o, nmethod.o, ...) references two symbols whose
// definitions live ONLY in os_cpu/bsd_aarch64/os_bsd_aarch64.cpp — a file the ZERO
// variant does not compile (it builds os_cpu/bsd_zero instead, and os_bsd_zero.cpp
// defines only os::current_thread_enable_wx). The static archive therefore has
// exactly two dangling references (verified by a full cross-archive nm audit):
//
//   __ZN2os17_jit_exec_enabledE            (THREAD_LOCAL bool os::_jit_exec_enabled)
//   __ZN2os27thread_wx_enable_write_implEv (void os::thread_wx_enable_write_impl())
//   _DefaultWXWriteMode                    (WXMode DefaultWXWriteMode, global)
//
// Semantics of this shim are EXACT for the Zero-on-iOS build, not approximations:
// the zero port's os::current_thread_enable_wx never writes _jit_exec_enabled (and
// on __IOS__ is a complete no-op — pthread_jit_write_protect_np is not allowed),
// so the thread-local stays false for every thread, os::thread_wx_enable_write()
// short-circuits, and thread_wx_enable_write_impl() is never invoked. There are no
// MAP_JIT pages in an interpreter-only build to protect in the first place.
//
// The variable MUST be a genuine Mach-O thread-local (TLV descriptor): referencing
// objects access it through the TLV thunk on every CodeHeap/nmethod write path, so
// a plain data symbol stand-in would crash. Hence C++/__thread here rather than a
// Rust #[export_name] (stable Rust cannot emit #[thread_local] statics).
//
// Compiled and linked (iOS DEVICE target only) by build.rs.

#define THREAD_LOCAL __thread

// Mirrors hotspot os.hpp's WXMode; upstream's definition (os_bsd_aarch64.cpp)
// is a plain zero-initialized global, i.e. WXWrite. Read only on error/assert
// (.cold) paths in the release archive.
enum WXMode {
  WXWrite = 0,
  WXExec = 1,
  WXArmedForWrite = 2,
};

WXMode DefaultWXWriteMode;

// Only the mangled names matter: class name + static member names mirror
// hotspot's `class os`; member privacy/order do not affect Itanium mangling.
class os {
 public:
  static THREAD_LOCAL bool _jit_exec_enabled;
  static void thread_wx_enable_write_impl();
};

THREAD_LOCAL bool os::_jit_exec_enabled = false;

void os::thread_wx_enable_write_impl() {
  // Unreachable in this build (see header comment); keep the tracked state
  // consistent with "write enabled" if it ever runs.
  _jit_exec_enabled = false;
}
