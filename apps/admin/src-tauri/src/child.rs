//! The engine and the server must not outlive the app, even when it is force-killed
//! (no `RunEvent::Exit`, no `kill_on_drop`). Each OS gets its closest reliable tool:
//!
//! * **Linux**: `PR_SET_PDEATHSIG`. The kernel sends the child SIGTERM (both the server
//!   and the JVM shut down cleanly on it) when the thread that spawned it ends. Children
//!   are spawned from the async runtime's worker threads, which live as long as the app.
//! * **Windows**: every child joins one Job Object created with
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. The app holds the only handle; when the app
//!   dies, the OS closes it and ends every process in the job.
//! * **macOS** (and other Unix): no kernel mechanism exists without a helper process, so
//!   children are recorded in a pid file and the next launch stops any that were left
//!   behind ([`reap_leftovers`]), before it starts new ones on the same data.
//!
//! The engine is spawned by the reader's supervisor; these hooks reach it through
//! `suwayomi::set_child_hooks`. The server is spawned in src/local.rs.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The pid file, set once by [`init`].
static PID_FILE: OnceLock<PathBuf> = OnceLock::new();

/// Executable names a recorded pid must still have to be stopped (pid reuse guard).
pub const CHILD_NAMES: [&str; 3] = ["komika-server", "java", "java.exe"];

/// Set up for this run: stop children a killed previous run left behind (Unix), then
/// start recording this run's.
pub fn init(data_dir: &Path) {
  let file = data_dir.join("children.pid");
  #[cfg(unix)]
  reap_leftovers(&file, &CHILD_NAMES);
  let _ = std::fs::create_dir_all(data_dir);
  let _ = std::fs::write(&file, "");
  let _ = PID_FILE.set(file);
}

/// Before `spawn`, for every child of the app.
pub fn before_spawn(cmd: &mut tokio::process::Command) {
  #[cfg(target_os = "linux")]
  linux::die_with_parent(cmd);
  #[cfg(not(target_os = "linux"))]
  let _ = cmd;
}

/// Right after `spawn`, for every child of the app.
pub fn after_spawn(child: &tokio::process::Child) {
  #[cfg(windows)]
  windows::adopt(child);
  if let (Some(file), Some(pid)) = (PID_FILE.get(), child.id()) {
    record(file, pid);
  }
}

/// Clean exit: every child was stopped, so nothing is left to reap next time.
pub fn forget_all() {
  if let Some(file) = PID_FILE.get() {
    let _ = std::fs::write(file, "");
  }
}

fn record(file: &Path, pid: u32) {
  use std::io::Write;
  if let Ok(mut f) = std::fs::OpenOptions::new()
    .create(true)
    .append(true)
    .open(file)
  {
    let _ = writeln!(f, "{pid}");
  }
}

/// Stop processes recorded in `file` that are still running as one of `names` and were
/// orphaned (their parent is gone, so they were re-parented to pid 1). Uses `ps` and
/// `kill`, like the shared supervisor, rather than a libc dependency. Returns the pids
/// it stopped.
#[cfg(unix)]
pub fn reap_leftovers(file: &Path, names: &[&str]) -> Vec<u32> {
  let Ok(text) = std::fs::read_to_string(file) else {
    return Vec::new();
  };
  let mut stopped = Vec::new();
  for pid in text.lines().filter_map(|l| l.trim().parse::<u32>().ok()) {
    if pid <= 1 || !is_orphan_named(pid, names) {
      continue;
    }
    log::warn!("stopping a process left behind by the last run (pid {pid})");
    signal(pid, "-TERM");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while alive(pid) && std::time::Instant::now() < deadline {
      std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if alive(pid) {
      signal(pid, "-KILL");
    }
    stopped.push(pid);
  }
  stopped
}

/// `ps -o ppid=,comm= -p <pid>`: parent 1 and an executable named like one of `names`.
#[cfg(unix)]
fn is_orphan_named(pid: u32, names: &[&str]) -> bool {
  let Ok(out) = std::process::Command::new("ps")
    .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
    .output()
  else {
    return false;
  };
  let text = String::from_utf8_lossy(&out.stdout);
  let mut parts = text.split_whitespace();
  let (Some(ppid), Some(comm)) = (parts.next(), parts.next()) else {
    return false;
  };
  let base = Path::new(comm)
    .file_name()
    .map(|n| n.to_string_lossy().into_owned())
    .unwrap_or_default();
  ppid == "1" && names.iter().any(|n| base == *n)
}

#[cfg(unix)]
fn signal(pid: u32, sig: &str) {
  let _ = std::process::Command::new("kill")
    .args([sig, &pid.to_string()])
    .stderr(std::process::Stdio::null())
    .status();
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
  std::process::Command::new("kill")
    .args(["-0", &pid.to_string()])
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

#[cfg(target_os = "linux")]
mod linux {
  /// Ask the kernel for SIGTERM when the spawning thread ends. If the app already died
  /// between fork and this call (the parent changed), exit instead of running orphaned.
  pub fn die_with_parent(cmd: &mut tokio::process::Command) {
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: only async-signal-safe calls (prctl, getppid) between fork and exec.
    unsafe {
      cmd.pre_exec(move || {
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
          return Err(std::io::Error::last_os_error());
        }
        if libc::getppid() != parent {
          return Err(std::io::Error::other(
            "the app exited while starting a child",
          ));
        }
        Ok(())
      });
    }
  }
}

#[cfg(windows)]
mod windows {
  use std::sync::OnceLock;
  use windows_sys::Win32::Foundation::HANDLE;
  use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
  };

  /// The job's handle as an integer (raw handles are not `Sync`). Never closed: the OS
  /// closes it when the app's process ends, which is what kills the children.
  static JOB: OnceLock<Option<usize>> = OnceLock::new();

  fn job() -> Option<HANDLE> {
    let raw = JOB.get_or_init(|| {
      // SAFETY: plain Win32 calls with a zeroed POD struct of the right size.
      unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
          log::warn!("could not create the job object for child processes");
          return None;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
          job,
          JobObjectExtendedLimitInformation,
          &info as *const _ as *const core::ffi::c_void,
          std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
          log::warn!("could not configure the job object for child processes");
          return None;
        }
        Some(job as usize)
      }
    });
    raw.map(|h| h as HANDLE)
  }

  pub fn adopt(child: &tokio::process::Child) {
    let (Some(job), Some(process)) = (job(), child.raw_handle()) else {
      return;
    };
    // SAFETY: both handles are valid for the duration of the call.
    if unsafe { AssignProcessToJobObject(job, process as HANDLE) } == 0 {
      log::warn!("could not tie a child process to the app (job object)");
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("komiq-child-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
  }

  /// A `sleep` whose parent shell has exited, i.e. re-parented like an app child after
  /// a force-kill. Returns its pid.
  #[cfg(unix)]
  fn orphan_sleep() -> u32 {
    let out = std::process::Command::new("sh")
      .args(["-c", "sleep 60 >/dev/null 2>&1 & echo $!"])
      .output()
      .unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
  }

  #[cfg(unix)]
  #[test]
  fn leftovers_are_stopped_only_when_orphaned_and_named() {
    let dir = tmp("reap");
    let file = dir.join("children.pid");
    let orphan = orphan_sleep();
    // Our own live, non-orphaned child must never be touched, whatever its name.
    let mut own = std::process::Command::new("sleep")
      .arg("60")
      .spawn()
      .unwrap();
    std::fs::write(&file, format!("{orphan}\n{}\nnot-a-pid\n1\n", own.id())).unwrap();

    // Name guard: a recorded pid running something else is left alone.
    assert!(reap_leftovers(&file, &["komika-server"]).is_empty());
    assert!(alive(orphan));

    let reparented_to_init = is_orphan_named(orphan, &["sleep"]);
    eprintln!("orphan re-parented to pid 1: {reparented_to_init}");
    let stopped = reap_leftovers(&file, &["sleep"]);
    if reparented_to_init {
      assert_eq!(stopped, vec![orphan]);
      assert!(!alive(orphan), "left-behind process stopped");
    } else {
      // A subreaper (e.g. a service manager) adopted it: not provably ours, kept.
      assert!(stopped.is_empty());
      signal(orphan, "-KILL");
    }
    assert!(own.try_wait().unwrap().is_none(), "own child untouched");
    let _ = own.kill();
    let _ = own.wait();
    let _ = std::fs::remove_dir_all(&dir);
  }

  #[test]
  fn init_records_and_forgets() {
    let dir = tmp("record");
    init(&dir);
    let file = PID_FILE.get().unwrap().clone();
    record(&file, 4242);
    record(&file, 4243);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "4242\n4243\n");
    forget_all();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "");
    let _ = std::fs::remove_dir_all(&dir);
  }

  /// PR_SET_PDEATHSIG fires when the spawning THREAD ends, so a thread that spawns a
  /// child and exits stands in for a killed app.
  #[cfg(target_os = "linux")]
  #[test]
  fn linux_child_dies_with_its_spawner() {
    // One child with the hook, one without (the control), from a thread that then ends.
    let (guarded, control) = std::thread::spawn(|| {
      let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
      rt.block_on(async {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("60");
        before_spawn(&mut cmd);
        let guarded = cmd.spawn().unwrap();
        let control = tokio::process::Command::new("sleep")
          .arg("60")
          .spawn()
          .unwrap();
        let pids = (guarded.id().unwrap(), control.id().unwrap());
        // Not killed by us: forget the handles so only the kernel can end them.
        std::mem::forget(guarded);
        std::mem::forget(control);
        pids
      })
    })
    .join()
    .unwrap();
    // A dead child stays a zombie until reaped: count only non-zombie states as running.
    let running = |pid: u32| {
      std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .map(|o| {
          let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
          !s.is_empty() && !s.starts_with('Z')
        })
        .unwrap_or(false)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while running(guarded) && std::time::Instant::now() < deadline {
      std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!running(guarded), "child ended with its spawner");
    assert!(running(control), "a child without the hook outlives it");
    signal(control, "-KILL");
  }
}
