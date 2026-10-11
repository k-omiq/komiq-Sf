//! Process image, creation identity and exact -D ownership. Failure means no signal.
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFile {
  pub pid: u32,
  pub data: PathBuf,
  /// Line 3: when the server that wrote the file was started, in seconds since 1970
  /// (PostgreSQL's `MyStartTime`). `None`: no such line, or not a number. It ties the
  /// file to one process: a later process that was given the same number was created at
  /// another time ([`start_agrees`]).
  pub started: Option<i64>,
}
pub fn parse_pid(text: &str) -> Result<PidFile, String> {
  parse_pid_as(text, cfg!(windows))
}

/// The first two lines of a `postmaster.pid`, read by the rules of Windows or of Unix.
/// Which rules is a parameter, so both are tested on every system. `lines` also takes
/// the `\r\n` line ends of a file that Windows wrote in text mode.
fn parse_pid_as(text: &str, windows: bool) -> Result<PidFile, String> {
  let mut lines = text.lines();
  let pid = lines
    .next()
    .and_then(|s| s.parse::<u32>().ok())
    .filter(|p| *p > 1)
    .ok_or("Invalid postmaster.pid PID; nothing was stopped")?;
  let data = lines
    .next()
    .filter(|s| !s.is_empty())
    .ok_or("Invalid postmaster.pid directory; nothing was stopped")?;
  if !names_one_folder(data, windows) {
    return Err("postmaster.pid directory is not absolute".into());
  }
  Ok(PidFile {
    pid,
    data: PathBuf::from(data),
    started: lines.next().and_then(|s| s.trim().parse().ok()),
  })
}

/// Whether the data folder line of a `postmaster.pid` names one folder, whatever the
/// working directory and the current drive of the reader are.
///
/// Unix: PostgreSQL writes the absolute path, which starts with `/`.
///
/// Windows: PostgreSQL 18 writes its data directory after `make_absolute_path` and
/// `canonicalize_path`, so with FORWARD slashes: `C:/Users/…/data` for a drive path and
/// `//server/share/…/data` for a network path. A verbatim path (`\\?\C:\…`) that was
/// given to it is written as `//?/C:/…`. All of these are taken, with either separator.
/// Not taken, though PostgreSQL's own `is_absolute_path` takes it: a path with a root but
/// no drive (`/data`, `\data`). Which drive it means depends on the reader's current
/// drive, so it cannot be compared with the app's data folder. The app always gives
/// PostgreSQL a drive or share path, so PostgreSQL never writes this form for the app's
/// own database; a file with it is treated as not usable, and PostgreSQL decides.
/// Never taken: `C:data` (relative to a drive's working directory) and relative paths.
fn names_one_folder(text: &str, windows: bool) -> bool {
  if !windows {
    return text.starts_with('/');
  }
  let separator = |byte: u8| byte == b'/' || byte == b'\\';
  let bytes = text.as_bytes();
  if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && separator(bytes[2]) {
    return true;
  }
  if bytes.len() > 2 && separator(bytes[0]) && separator(bytes[1]) {
    // Both first bytes are ASCII, so the text can be cut after them.
    let mut parts = text[2..].split(['/', '\\']);
    let named = |part: Option<&str>| part.is_some_and(|part| !part.is_empty());
    return named(parts.next()) && named(parts.next());
  }
  false
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Identity {
  pub pid: u32,
  pub image: PathBuf,
  pub created: String,
  pub args: Vec<String>,
}

pub fn owns(identity: &Identity, executable: &Path, data: &Path, pid: &PidFile) -> bool {
  let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
    (Ok(a), Ok(b)) => a == b,
    _ => false,
  };
  identity.pid == pid.pid
    && same(&identity.image, executable)
    && same(&pid.data, data)
    && identity
      .args
      .windows(2)
      .any(|args| args[0] == "-D" && same(Path::new(&args[1]), data))
}

/// What holds a PID, read without the process's arguments (cheap on every system; on
/// Windows no helper program is started).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Holder {
  /// No process has this PID.
  Free,
  /// A process that has ended and only waits to be collected by its parent. It runs
  /// nothing and holds no data directory. (Windows reports such a PID as free; the
  /// variant is only made there by the tests of `claim`, which run on every system.)
  #[cfg_attr(windows, allow(dead_code))]
  Ended,
  /// A live process started from this program file.
  Program(PathBuf),
  /// A live process of this user whose program file no longer exists (macOS: the app was
  /// deleted or moved to another disk while its database ran). It still runs, and
  /// PostgreSQL can signal it. Linux keeps naming the file, with ` (deleted)`, and
  /// Windows does not let a running program file go, so only macOS makes this variant
  /// (and the tests of `claim`, which run on every system).
  #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
  Gone(String),
  /// A live process this user may not look at (another user's, or the system's), or one
  /// whose program file could not be read for another reason. What it is, is not known.
  Unreadable(String),
}

#[cfg(target_os = "macos")]
pub fn holder(pid: u32) -> Holder {
  match program_file(pid) {
    Ok(image) => Holder::Program(image),
    Err(error) => match error.raw_os_error() {
      // Also a process that has ended and waits to be collected: it has no program file.
      Some(libc::ESRCH) if exists(pid) => Holder::Ended,
      Some(libc::ESRCH) => Holder::Free,
      Some(libc::ENOENT) => Holder::Gone(format!(
        "the program file of process {pid} no longer exists"
      )),
      _ => Holder::Unreadable(format!("Cannot inspect process {pid}: {error}")),
    },
  }
}

#[cfg(target_os = "linux")]
pub fn holder(pid: u32) -> Holder {
  linux_holder(Path::new("/proc"), pid)
}

#[cfg(windows)]
pub fn holder(pid: u32) -> Holder {
  match image(pid) {
    Ok(Some(image)) => Holder::Program(image),
    // `image` opened the process itself: no such PID, or its exit code is set.
    Ok(None) => Holder::Free,
    Err(error) => Holder::Unreadable(error),
  }
}

/// Linux, from `proc` (`/proc`; a made-up tree in the tests). A process has ended only
/// when it has no directory there (`Free`) or its `stat` shows state `Z` (`Ended`). A
/// live process whose `exe` link cannot be read (a kernel thread, another user's
/// process, a hardened system) is unknown, never ended.
///
/// Compiled for Linux, and for the tests of the other Unix systems (the made-up tree
/// uses symbolic links, so its test is a Unix test). Not for Windows.
#[cfg(any(target_os = "linux", all(unix, test)))]
pub(crate) fn linux_holder(proc: &Path, pid: u32) -> Holder {
  let root = proc.join(pid.to_string());
  let stat = match std::fs::read_to_string(root.join("stat")) {
    Ok(stat) => stat,
    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Holder::Free,
    // ESRCH: it ended while its entry was being read.
    Err(e) if e.raw_os_error() == Some(3) => return Holder::Free,
    Err(e) => return Holder::Unreadable(format!("Cannot inspect process {pid}: {e}")),
  };
  // `pid (name) state …`; the name may itself contain spaces and parentheses.
  let state = stat
    .rfind(')')
    .and_then(|at| stat[at + 1..].split_whitespace().next());
  match state {
    Some("Z") | Some("X") => return Holder::Ended,
    Some(_) => {}
    None => {
      return Holder::Unreadable(format!(
        "Cannot inspect process {pid}: its state could not be read"
      ))
    }
  }
  match linux_program(&root) {
    Ok((image, _)) => Holder::Program(image),
    Err(e) => Holder::Unreadable(format!(
      "Cannot read the program file of the live process {pid}: {e}"
    )),
  }
}

/// The program a Linux process runs, from its `/proc` entry `root`: the `exe` link.
///
/// One exception. Under user-mode emulation (a program of another processor type run
/// through `qemu-user` and binfmt_misc) the link names the emulator, and the program
/// itself is one of the first arguments, as an absolute path. A PostgreSQL server run
/// that way would count as "another program", and PostgreSQL would be told to disregard
/// it. So when the link is not a PostgreSQL name and one of the first three arguments is
/// an absolute path whose file is called exactly `postgres` or `postmaster`, that
/// argument is the program, and the second value is `true`: the name was read from the
/// arguments, which any program can be given (a script `/tmp/postgres` run by an
/// interpreter looks the same). Never run under emulation here.
///
/// What such a name is good for: only to be careful. In the look at the lock file it
/// turns "another program" into "a PostgreSQL", which PostgreSQL is then not told to
/// disregard. In the look before a start it can refuse the start; it never lets a signal
/// be sent ([`strays`]).
#[cfg(any(target_os = "linux", all(unix, test)))]
fn linux_program(root: &Path) -> std::io::Result<(PathBuf, bool)> {
  use std::os::unix::ffi::OsStrExt;
  let image = std::fs::read_link(root.join("exe"))?;
  if postgres_like(&image) {
    return Ok((image, false));
  }
  let emulated = std::fs::read(root.join("cmdline")).ok().and_then(|bytes| {
    bytes
      .split(|b| *b == 0)
      .take(3)
      .map(|arg| PathBuf::from(std::ffi::OsStr::from_bytes(arg)))
      .find(|arg| {
        arg.is_absolute()
          && arg
            .file_name()
            .is_some_and(|name| name == "postgres" || name == "postmaster")
      })
  });
  Ok(match emulated {
    Some(program) => (program, true),
    None => (image, false),
  })
}

#[cfg(target_os = "macos")]
fn exists(pid: u32) -> bool {
  // SAFETY: signal 0 only asks whether the PID is in use; nothing is delivered.
  let delivered = unsafe { libc::kill(pid as i32, 0) } == 0;
  delivered || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The program file of the live process `pid`, as the system reports it now.
/// `ESRCH`: no live process. `ENOENT`: the file no longer exists.
#[cfg(target_os = "macos")]
fn program_file(pid: u32) -> std::io::Result<PathBuf> {
  extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut core::ffi::c_void, size: u32) -> i32;
  }
  let mut image = [0u8; 4096];
  // SAFETY: buffer is writable for the declared length, proc_pidpath does not retain it.
  let len = unsafe { proc_pidpath(pid as i32, image.as_mut_ptr().cast(), image.len() as u32) };
  if len <= 0 {
    return Err(std::io::Error::last_os_error());
  }
  let end = image.iter().position(|b| *b == 0).unwrap_or(image.len());
  use std::os::unix::ffi::OsStringExt;
  Ok(PathBuf::from(std::ffi::OsString::from_vec(
    image[..end].to_vec(),
  )))
}

/// The program file of process `pid`. `Ok(None)`: no live process has this PID.
#[cfg(all(unix, feature = "desktop-gate"))]
pub fn image(pid: u32) -> Result<Option<PathBuf>, String> {
  match holder(pid) {
    Holder::Program(image) => Ok(Some(image)),
    Holder::Free | Holder::Ended => Ok(None),
    Holder::Gone(error) | Holder::Unreadable(error) => Err(error),
  }
}

/// The program file of process `pid`. `Ok(None)`: no live process has this PID.
#[cfg(windows)]
pub fn image(pid: u32) -> Result<Option<PathBuf>, String> {
  use windows_sys::Win32::Foundation::CloseHandle;
  use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
  };
  // SAFETY: the handle is closed on every path; outputs are initialized buffers.
  unsafe {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
      let error = std::io::Error::last_os_error();
      // ERROR_INVALID_PARAMETER: no process has this PID.
      if error.raw_os_error() == Some(87) {
        return Ok(None);
      }
      return Err(format!("Cannot inspect process {pid}: {error}"));
    }
    let result = (|| {
      let mut code = 0;
      if GetExitCodeProcess(handle, &mut code) == 0 {
        return Err(std::io::Error::last_os_error().to_string());
      }
      if code != 259 {
        return Ok(None); // Not STILL_ACTIVE: it has ended.
      }
      let mut buffer = vec![0u16; 32768];
      let mut size = buffer.len() as u32;
      if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) == 0 {
        return Err(std::io::Error::last_os_error().to_string());
      }
      Ok(Some(PathBuf::from(String::from_utf16_lossy(
        &buffer[..size as usize],
      ))))
    })();
    CloseHandle(handle);
    result
  }
}

/// Where a process runs, compared with a data folder by the identity of the folder
/// (device and inode), never by a path text. A PostgreSQL main process changes into its
/// data directory before it creates the lock file and stays there, so its working folder
/// IS its data directory, whatever its arguments say and whatever the folder is called
/// by now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
  /// The working folder of the process is this data folder.
  Here,
  /// It was read and is another folder (its path now, when the system gives one).
  /// Never made on Windows, where the working folder of a process is not read.
  #[cfg_attr(not(unix), allow(dead_code))]
  Elsewhere(Option<PathBuf>),
  /// It could not be read. Nothing is proven.
  Unknown(String),
}

/// A folder as the file system knows it.
#[cfg(any(unix, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderId {
  pub device: u64,
  pub inode: u64,
}

/// [`Place`] from the two readings. `data`: the data folder. `working`: the working
/// folder of the process and its path; `Ok(None)`: the process is gone.
#[cfg(any(unix, test))]
pub fn placed(
  data: Result<FolderId, String>,
  working: Result<Option<(FolderId, Option<PathBuf>)>, String>,
) -> Place {
  let data = match data {
    Ok(data) => data,
    Err(error) => return Place::Unknown(error),
  };
  match working {
    Ok(Some((folder, _))) if folder == data => Place::Here,
    Ok(Some((_, path))) => Place::Elsewhere(path),
    Ok(None) => Place::Unknown("the process ended while it was looked at".into()),
    Err(error) => Place::Unknown(error),
  }
}

/// Whether process `pid` runs in the folder `data`.
pub fn place(pid: u32, data: &Path) -> Place {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let folder = std::fs::metadata(data)
      .map(|meta| folder_id(meta.dev(), meta.ino()))
      .map_err(|e| format!("{}: {e}", data.display()));
    placed(folder, working_folder(pid))
  }
  #[cfg(not(unix))]
  {
    let _ = (pid, data);
    Place::Unknown("the working folder of a process is not read on this system".into())
  }
}

/// macOS reports the device of a process's working folder in 32 bits; `stat` gives the
/// same number, so both are cut to that width before they are compared.
#[cfg(unix)]
fn folder_id(device: u64, inode: u64) -> FolderId {
  FolderId {
    device: if cfg!(target_os = "macos") {
      device & 0xffff_ffff
    } else {
      device
    },
    inode,
  }
}

/// The working folder of process `pid`. `Ok(None)`: no such process.
#[cfg(target_os = "macos")]
fn working_folder(pid: u32) -> Result<Option<(FolderId, Option<PathBuf>)>, String> {
  // SAFETY: a plain C struct of integers and byte arrays; all zeroes is a valid value.
  let mut info: libc::proc_vnodepathinfo = unsafe { std::mem::zeroed() };
  let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as i32;
  // SAFETY: writable buffer of exactly the declared size; proc_pidinfo retains nothing.
  let read = unsafe {
    libc::proc_pidinfo(
      pid as i32,
      libc::PROC_PIDVNODEPATHINFO,
      0,
      (&mut info as *mut libc::proc_vnodepathinfo).cast(),
      size,
    )
  };
  if read != size {
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
      return Ok(None);
    }
    return Err(format!(
      "Cannot read the working folder of process {pid}: {error}"
    ));
  }
  let stat = &info.pvi_cdir.vip_vi.vi_stat;
  if stat.vst_ino == 0 {
    return Err(format!("Process {pid} reports no working folder"));
  }
  let bytes: Vec<u8> = info
    .pvi_cdir
    .vip_path
    .iter()
    .flatten()
    .map(|c| *c as u8)
    .take_while(|b| *b != 0)
    .collect();
  use std::os::unix::ffi::OsStringExt;
  let path = (!bytes.is_empty()).then(|| PathBuf::from(std::ffi::OsString::from_vec(bytes)));
  Ok(Some((
    folder_id(u64::from(stat.vst_dev), stat.vst_ino),
    path,
  )))
}

/// The working folder of process `pid`. `Ok(None)`: no such process.
#[cfg(all(unix, not(target_os = "macos")))]
fn working_folder(pid: u32) -> Result<Option<(FolderId, Option<PathBuf>)>, String> {
  use std::os::unix::fs::MetadataExt;
  let link = format!("/proc/{pid}/cwd");
  // Through the link: the folder itself, also after it was renamed.
  match std::fs::metadata(&link) {
    Ok(meta) => Ok(Some((
      folder_id(meta.dev(), meta.ino()),
      std::fs::read_link(&link).ok(),
    ))),
    Err(_) if !Path::new(&format!("/proc/{pid}")).exists() => Ok(None),
    Err(e) => Err(format!(
      "Cannot read the working folder of process {pid}: {e}"
    )),
  }
}

/// A process that is not this install's PostgreSQL but has the PID that `postmaster.pid`
/// names. It is never signaled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Foreign {
  pub pid: u32,
  pub image: Option<PathBuf>,
  /// What the process is, as a clause for the log line and a failed start.
  pub what: String,
  /// Proven not to be a PostgreSQL on this data directory, so PostgreSQL may be told to
  /// disregard this PID when it reads the lock file (see `Database::start`; never on
  /// Windows). The proof is one of three: the program file is not a PostgreSQL program
  /// at all; the process has ended and was not collected; or it is a PostgreSQL whose
  /// working folder was read and is not this data folder, while no file that was looked
  /// at in this folder is shared with another folder ([`shared_files`]) and that process
  /// was not started on this data folder's path.
  pub disregard: bool,
}

impl Foreign {
  /// `process 4242 (/bin/sleep)`.
  pub fn named(&self) -> String {
    match &self.image {
      Some(image) => format!("process {} ({})", self.pid, image.display()),
      None => format!("process {}", self.pid),
    }
  }
}

/// The database of this data folder, still running although this app did not start it in
/// this run: a PostgreSQL main process whose working folder is this data folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
  pub identity: Identity,
  /// It runs the program file this install starts. Otherwise the app was moved,
  /// replaced or deleted while its database ran.
  pub own_program: bool,
}

/// Whose PID `postmaster.pid` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
  /// No process: the file is left over. PostgreSQL clears it itself at start.
  Stale,
  /// The bundled `postgres` of this install, running on this data directory by its
  /// arguments and the lock file, and (Unix) with this data folder as its working
  /// folder. The texts alone are the only proof there is on Windows.
  Ours(Identity),
  /// This folder's database by the identity of its working folder, whatever its
  /// arguments and the lock file say about paths.
  Here(Adopted),
  /// Another process has the PID now, or a PostgreSQL that is not provably this
  /// folder's. The file is treated as left over; PostgreSQL decides at start.
  Foreign(Foreign),
}

/// What [`claim`] is told about the system and about the lock file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockFacts {
  /// The working folder of a process can be read on this system (Unix). Where it cannot
  /// (Windows), the texts of the arguments and of the lock file are all there is.
  pub folders_read: bool,
  /// Why the files of this data folder are not known to be this folder's alone, as a
  /// clause; `None`: every file that was looked at has one name. See [`shared_files`].
  pub shared: Option<String>,
}

impl LockFacts {
  /// For the data folder of the lock file at `lock`, on this system.
  pub fn read(lock: &Path) -> Self {
    Self {
      folders_read: cfg!(unix),
      shared: shared_files(lock),
    }
  }
}

/// Whether the data folder of the lock file `lock` shares files with another folder, as
/// far as a cheap look can tell. A copy of a data folder made with hard links is the
/// same files under second names; the live database of the folder it was copied from
/// then runs "in another folder" on the very files of this one, and PostgreSQL must not
/// be told to disregard it.
///
/// Looked at: the lock file, the two folders `global/` and `pg_wal/` themselves, and
/// every file directly in them (in a new cluster of PostgreSQL 18 about 58 and 3
/// entries, so about 60 cheap readings, and only when the lock file names a live
/// process). `Some(reason)` when one of the files has more than one name, is a link, or
/// cannot be read, or when one of the two folders is itself a link or cannot be listed.
/// So a copy is also found when its lock file and its control file were replaced by
/// ordinary copies, and when it kept the link to a log folder outside the data folder.
///
/// This is asked only about a PostgreSQL of another folder. The app's own database
/// starts with a `pg_wal` that is a link as before.
///
/// The limits of this stand-in. Not detected: a copy that shares only files outside
/// these two folders (the relation files under `base/`, `pg_xact/` and the like, and
/// tablespaces, which a copy reaches through the links in `pg_tblspc/`), with the lock
/// file and everything directly in `global/` and `pg_wal/` an ordinary copy. And not
/// detected: files of the two folders that are shared without a second name and
/// without a link, which is one folder mounted at two places.
fn shared_files(lock: &Path) -> Option<String> {
  #[cfg(unix)]
  {
    use std::os::unix::fs::MetadataExt;
    let together = "so this folder shares its files with another folder and that process can be \
                    the database of both";
    let look = |file: &Path, shown: &str| -> Option<String> {
      match std::fs::symlink_metadata(file) {
        Ok(meta) if meta.is_dir() => None,
        Ok(meta) if meta.file_type().is_symlink() => Some(format!(
          "the file {shown} of this data folder is a link to another file, {together}"
        )),
        Ok(meta) if meta.nlink() == 1 => None,
        Ok(meta) => Some(format!(
          "the file {shown} of this data folder has {} names (hard links), {together}",
          meta.nlink()
        )),
        Err(error) => Some(format!(
          "how many names the file {shown} of this data folder has could not be read ({error})"
        )),
      }
    };
    if let Some(reason) = look(lock, "postmaster.pid") {
      return Some(reason);
    }
    for folder in ["global", "pg_wal"] {
      let unread = |error: std::io::Error| {
        format!(
          "the folder {folder} of this data folder could not be read ({error}), so it is not \
           known whether its files are shared with another folder"
        )
      };
      // The folder itself first. A link here (PostgreSQL supports `pg_wal` as a link to
      // a folder on another disk) is kept by a copy of the data folder: then both data
      // folders reach the same files through it, and each of those files has one name.
      // Listing would follow the link and find nothing.
      match std::fs::symlink_metadata(lock.with_file_name(folder)) {
        Ok(meta) if meta.file_type().is_symlink() => {
          return Some(format!(
            "the folder {folder} of this data folder is a link to another folder, so its \
             files can be the files of another data folder as well, and that process can \
             be the database of both"
          ))
        }
        Ok(_) => {}
        Err(error) => return Some(unread(error)),
      }
      let entries = match std::fs::read_dir(lock.with_file_name(folder)) {
        Ok(entries) => entries.collect::<Result<Vec<_>, _>>(),
        Err(error) => Err(error),
      };
      let mut entries = match entries {
        Ok(entries) => entries,
        Err(error) => return Some(unread(error)),
      };
      // In the order of their names, so that the same file is named every time.
      entries.sort_by_key(|entry| entry.file_name());
      for entry in entries {
        let shown = format!("{folder}/{}", entry.file_name().to_string_lossy());
        if let Some(reason) = look(&entry.path(), &shown) {
          return Some(reason);
        }
      }
    }
    None
  }
  // Not read on Windows: PostgreSQL is never told to disregard a process there.
  #[cfg(not(unix))]
  {
    let _ = lock;
    Some("the names of its files are not read on this system".into())
  }
}

/// `postgres`, `postgres.exe`, `postgres (deleted)` (Linux, replaced program file), and
/// the old name `postmaster`. Wide on purpose (`postgres_exporter` and `postgres-helper`
/// pass too), and for that reason good for one thing only: to be careful. A program with
/// such a name is never called "not PostgreSQL" (so PostgreSQL is not told to disregard
/// it), and it is looked at before a start. The name proves nothing about what the
/// program is: it never lets a process be signaled. That takes the proof of [`strays`]
/// (the lock file in the folder the process runs in) or of [`claim`] (the working folder
/// is this data folder).
pub(crate) fn postgres_like(image: &Path) -> bool {
  image.file_name().is_some_and(|name| {
    let name = name.to_string_lossy().to_ascii_lowercase();
    name.starts_with("postgres") || name.starts_with("postmaster")
  })
}

/// Decide what the PID in `postmaster.pid` is. `holder` is the cheap look at the PID;
/// `identity` (arguments and creation time) and `place` (the working folder) are asked
/// only for a program that looks like PostgreSQL or whose file is gone. All three are
/// passed in so that every branch can be tested.
///
/// Nothing here stops a process or touches the file. A wrong "left over" is caught by
/// PostgreSQL itself: at start it refuses a data directory whose lock file names a live
/// process it may signal, and one whose shared memory is still in use.
pub fn claim(
  pid: &PidFile,
  executable: &Path,
  data: &Path,
  facts: LockFacts,
  holder: Holder,
  identity: impl FnOnce(u32) -> Result<Option<Identity>, String>,
  place: impl FnOnce(u32) -> Place,
) -> Claim {
  let foreign = |image: Option<PathBuf>, what: String, disregard: bool| {
    Claim::Foreign(Foreign {
      pid: pid.pid,
      image,
      what,
      disregard,
    })
  };
  let image = match holder {
    Holder::Free => return Claim::Stale,
    Holder::Ended => {
      return foreign(
        None,
        "a process that has ended and only waits to be collected".into(),
        true,
      )
    }
    Holder::Unreadable(error) => {
      // Nothing is known about it, so nothing is said to PostgreSQL. For a process of
      // another user PostgreSQL goes on by its own rule (it may not signal it, and a
      // database on this folder would be this user's). For any other it refuses.
      return foreign(
        None,
        format!("a process that could not be inspected ({error})"),
        false,
      );
    }
    Holder::Program(image) if !postgres_like(&image) => {
      return foreign(
        Some(image),
        "another program that was given this process number".into(),
        true,
      );
    }
    Holder::Program(image) => Some(image),
    // A live process of this user: PostgreSQL can signal it and would refuse the start.
    Holder::Gone(_) => None,
  };
  let current = match identity(pid.pid) {
    Ok(None) => return Claim::Stale, // it ended between the two looks
    Ok(Some(current)) => Ok(current),
    Err(error) => Err(error),
  };
  // For a program file that is gone: the name the process was started under.
  let shown = image
    .clone()
    .or_else(|| current.as_ref().ok().map(|c| c.image.clone()));
  if image.is_none() && !shown.as_deref().is_some_and(postgres_like) {
    return foreign(
      shown,
      "a program whose file no longer exists; what it is could not be told".into(),
      false,
    );
  }
  // This install's program, with this data folder in its `-D` argument and in the lock
  // file. On Windows that is all the proof there is. On Unix the texts are not enough:
  // after the app data folder was renamed and a copy of it was put at the old name, the
  // database of the renamed folder still shows the old path, and a stop request sent
  // through this folder's lock file would make it write its control file into this
  // folder. So the working folder decides there as well.
  let by_text = current
    .as_ref()
    .is_ok_and(|current| owns(current, executable, data, pid));
  if by_text && !facts.folders_read {
    if let Ok(current) = current {
      return Claim::Ours(current);
    }
  }
  let same = |a: &Path, b: &Path| match (a.canonicalize(), b.canonicalize()) {
    (Ok(a), Ok(b)) => a == b,
    _ => false,
  };
  // Any PostgreSQL, of this install or not, whose `-D` argument names this data folder.
  let started_here = current.as_ref().is_ok_and(|current| {
    current
      .args
      .windows(2)
      .any(|args| args[0] == "-D" && same(Path::new(&args[1]), data))
  });
  match place(pid.pid) {
    Place::Here => match current {
      Ok(current) if by_text => Claim::Ours(current),
      // A main process shows the arguments it was started with (this app always gives
      // `-D`). A helper process of a database shows a title instead; it is not stopped
      // in place of its main process.
      Ok(current) if current.args.iter().any(|arg| arg == "-D") => Claim::Here(Adopted {
        own_program: image
          .as_deref()
          .is_some_and(|image| same(image, executable)),
        identity: current,
      }),
      Ok(_) => foreign(
        shown,
        "a PostgreSQL process that runs in this data folder but was not started the way \
         this app starts its database"
          .into(),
        false,
      ),
      Err(error) => foreign(
        shown,
        format!(
          "a PostgreSQL process that runs in this data folder and could not be identified \
           ({error})"
        ),
        false,
      ),
    },
    // It was started on this data folder's path and does not run in this folder: the
    // folder it runs in was renamed, and this is another folder at the old path. It is
    // not a database on this folder, and it is not harmless either: PostgreSQL writes its
    // control file by the path it was started with, which is this folder now. The look
    // before a start (`strays`) ends such a process before anything is started here; one
    // that is met here all the same is never disregarded, so PostgreSQL refuses.
    Place::Elsewhere(folder) if started_here => foreign(
      shown,
      format!(
        "a PostgreSQL that was started on this data folder's path and now runs in another \
         folder{}; it has to be ended before a database can run here",
        folder
          .map(|folder| format!(" ({})", folder.display()))
          .unwrap_or_default()
      ),
      false,
    ),
    // Read, and not this folder: it is not a database on this folder. Unless this folder
    // shares files with another folder (a copy made with hard links): then the process
    // can be that folder's live database. PostgreSQL's own refusal of the lock file is
    // then the only thing between two servers and one set of files, so nothing is said
    // to it. (What the look at the files cannot see: `shared_files`.)
    Place::Elsewhere(folder) => {
      let other = match folder {
        Some(folder) => format!("a PostgreSQL of another data folder ({})", folder.display()),
        None => "a PostgreSQL of another data folder".into(),
      };
      match facts.shared {
        None => foreign(shown, other, true),
        Some(shared) => foreign(shown, format!("{other}; {shared}"), false),
      }
    }
    Place::Unknown(error) => foreign(
      shown,
      format!("a PostgreSQL process whose data folder could not be read ({error})"),
      false,
    ),
  }
}

// ---- the look before a start: a database of this data path that runs elsewhere ---------

/// A live process of this user whose program is called like the PostgreSQL server, as the
/// look before a start sees it.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
  pub pid: u32,
  pub parent: u32,
  /// Its program file, or the path it was started with when that file is gone.
  pub program: PathBuf,
  /// When it was created, as [`Identity::created`] gives it.
  pub created: String,
  /// The data directory it was started on, as the text it was given: its `-D` argument,
  /// otherwise the `PGDATA` of its environment. PostgreSQL makes this text absolute at
  /// start and keeps it for its whole life, whatever happens to the folder. With a
  /// `data_directory` setting this text names the folder of the configuration files, not
  /// the data directory: it is a reason to look, never the proof ([`strays`]).
  pub data: Option<String>,
  /// Every `data_directory` setting among its arguments (`-c data_directory=…`,
  /// `--data_directory=…`), as text. A server started with a separate folder for its
  /// configuration names its data here and not in `-D`. Like `data`: a reason to look
  /// and to refuse, never the proof.
  pub data_directory: Vec<String>,
  /// When the process was created, in seconds since 1970, to compare with line 3 of a
  /// lock file ([`start_agrees`]). `None`: it could not be read.
  pub started: Option<i64>,
  /// The program name was read from the arguments of the process, not from what the
  /// system says it runs (Linux: a program run through an emulator, or any script that
  /// is called `postgres`). Such a process is never signaled.
  pub named_by_arguments: bool,
}

/// The data directory text of a PostgreSQL started with `args` (the program first) and
/// `pgdata` in its environment. `-D` wins over `PGDATA`, and the last `-D` wins.
#[cfg(unix)]
pub(crate) fn data_text(args: &[String], pgdata: Option<String>) -> Option<String> {
  let mut given = None;
  let mut rest = args.iter().skip(1);
  while let Some(arg) = rest.next() {
    if arg == "-D" {
      if let Some(value) = rest.next() {
        given = Some(value.clone());
      }
    } else if let Some(value) = arg.strip_prefix("-D").filter(|value| !value.is_empty()) {
      given = Some(value.to_string());
    }
  }
  given.or(pgdata).filter(|text| !text.is_empty())
}

/// Every value of a `data_directory` setting among `args` (the program first), in the
/// three forms PostgreSQL takes on its command line: `-c data_directory=/x`,
/// `-cdata_directory=/x` and `--data_directory=/x` (it reads `-` in a name as `_`, and
/// names without regard to case).
#[cfg(unix)]
pub(crate) fn data_directory_texts(args: &[String]) -> Vec<String> {
  let setting = |text: &str| -> Option<String> {
    let (name, value) = text.split_once('=')?;
    let name = name.trim().replace('-', "_");
    (name.eq_ignore_ascii_case("data_directory") && !value.is_empty()).then(|| value.to_string())
  };
  let mut found = Vec::new();
  let mut rest = args.iter().skip(1);
  while let Some(arg) = rest.next() {
    let value = if arg == "-c" {
      rest.next().and_then(|next| setting(next))
    } else if let Some(long) = arg.strip_prefix("--") {
      setting(long)
    } else {
      arg.strip_prefix("-c").and_then(setting)
    };
    found.extend(value);
  }
  found
}

/// Whether line 3 of a lock file (`file`: the start of the server that wrote it) and
/// the creation time of a process (`process`) are the same start, both in seconds since
/// 1970. `Err`: why not, as a clause.
///
/// PostgreSQL takes the time for the file as one of the first things it does
/// (`MyStartTime`), so it is the creation time of the process or a moment after it. The
/// span taken as "the same start" is from 5 seconds before to 60 seconds after the
/// creation: before, because Linux gives the creation time as the time of the boot plus
/// a count of ticks, which rounds and follows small corrections of the clock; after,
/// for a start on a machine so loaded that the program came up slowly. The span rests on
/// an assumption, which the code does not check: that the number of a server which
/// ended is not given to another process within a minute of that server's start. How
/// soon a number comes back depends on the range of numbers the system uses and on how
/// many processes are started. A clock that was set by more than this span after the
/// server started (Linux) makes a true pair disagree: then nothing is signaled and the
/// start is refused, which is the safe side. The refusal says that the two times
/// disagree and gives both; it does not say whose file it is.
#[cfg(unix)]
pub(crate) fn start_agrees(file: Option<i64>, process: Option<i64>) -> Result<(), String> {
  const BEFORE: i64 = 5;
  const AFTER: i64 = 60;
  match (file, process) {
    (Some(file), Some(process)) if (-BEFORE..=AFTER).contains(&(file - process)) => Ok(()),
    (Some(file), Some(process)) => Err(format!(
      "gives {file} (seconds since 1970) as the time its server was started, and this \
       process was created at {process}: the two times do not agree, so the file does not \
       show that this process wrote it"
    )),
    (None, _) => Err("has no start time in its third line".into()),
    (_, None) => Err(
      "cannot be tied to this process, because the time this process was created could not \
       be read"
        .into(),
    ),
  }
}

/// `path` with every part that exists resolved (links, `/tmp` on macOS), and the part
/// that does not exist yet kept as text. Two texts that name the same place compare equal
/// after this, also when the last folders are not there (a new install).
#[cfg(unix)]
pub(crate) fn resolved(path: &Path) -> PathBuf {
  let mut missing = Vec::new();
  let mut at = path;
  loop {
    if let Ok(real) = at.canonicalize() {
      return missing
        .iter()
        .rev()
        .fold(real, |path, part| path.join(part));
    }
    match (at.parent(), at.file_name()) {
      (Some(parent), Some(name)) => {
        missing.push(name.to_os_string());
        at = parent;
      }
      _ => return path.to_path_buf(),
    }
  }
}

/// What was read where a process runs, and what a signal to it rests on: the folder
/// (device and inode) and the first three lines of the lock file in it. Read again, and
/// compared with this, right before every signal ([`prove_again`]).
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
  pub folder: FolderId,
  pub lock: PidFile,
}

/// A process with a PostgreSQL name that has to do with the data path `data` and does not
/// run in the folder that is at that path now.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stray {
  /// Proven to be a PostgreSQL main process that endangers the data path (see
  /// [`strays`]). `folder`: the folder it runs in, by its path now. `evidence`: what
  /// was read there. `helpers`: the processes it started. Only such a process may be
  /// signaled, and only while the evidence still holds.
  Elsewhere {
    process: Seen,
    folder: PathBuf,
    evidence: Evidence,
    helpers: Vec<Seen>,
  },
  /// Not proven, and not cleared either: its arguments or environment name the data
  /// path, or the lock file where it runs does, and one part of the proof is missing
  /// (`reason`, a clause). It is never signaled; the start is refused with its number.
  Unproven { process: Seen, reason: String },
}

/// Which of the processes `seen` (PostgreSQL by name) endanger the data path `data`.
///
/// PostgreSQL does its work relative to its working folder, which follows a rename of
/// the folder. Its control file is the exception: it is written by the absolute path of
/// the data directory as it was at the start. A server whose data folder was renamed
/// therefore writes its control file into whatever database lies at the old path at its
/// next checkpoint or shutdown, and that database does not start again. The lock file at
/// the data path does not protect against this, because it moved away with the folder.
///
/// The proof that a process is such a server, and the only thing that lets it be
/// signaled ([`Stray::Elsewhere`]), has four parts:
/// - the folder the process runs in was read and is another folder than the one at
///   `data` (`place`);
/// - the `postmaster.pid` in that folder (`lock`) has this very process number in its
///   first line;
/// - its second line, the data directory, names the place of `data`;
/// - its third line, the start time of the server that wrote it, agrees with the time
///   this process was created ([`start_agrees`]). Without this a file that an earlier
///   server left behind would "prove" any later process that was given the same number
///   and runs in that folder.
///
/// A main process runs in its data directory and writes that file there with its own
/// number, with the data directory it acts on and with its start time, so the file also
/// tells the truth when the server was started with `-D <folder of the configuration>
/// -c data_directory=<data>`. The file is an ordinary file of this user. What is
/// guarded against is an accident and stale state, not another program of the same user
/// that writes such a file on purpose (it could end the process itself).
///
/// What is NOT proof: the name of the program (`postgres_exporter` has a PostgreSQL
/// name), and the `-D` argument, `PGDATA` or a `data_directory` setting (a tool can be
/// given them, and `-D` can name the configuration folder of a server whose data is
/// somewhere else). A process that those name, and that the lock file where it runs
/// does not prove, is [`Stray::Unproven`]: the start is refused with its process number
/// and nothing is sent to it. So is one whose working folder cannot be read, and one
/// that the lock file proves but whose program name was read from its arguments
/// (`Seen::named_by_arguments`). A process that neither the texts nor a lock file tie to
/// this data path is no business of this app.
///
/// `place`: where a process runs, compared with `data`; `None`: the process is gone. A
/// process in this very folder is this folder's own database and is left to the look at
/// the lock file ([`claim`]). A process whose parent is taken, or named by its texts, is
/// a helper of that server, not a server.
#[cfg(unix)]
pub fn strays(
  data: &Path,
  seen: &[Seen],
  place: impl Fn(u32) -> Option<Place>,
  lock: impl Fn(u32, &Path) -> Result<Evidence, String>,
) -> Vec<Stray> {
  enum Found {
    Proven(PathBuf, Evidence),
    Unproven(String),
  }
  let wanted = resolved(data);
  // A relative text cannot be compared: PostgreSQL made it absolute from a folder that
  // is not known any more. The app always gives an absolute path.
  let names = |text: &Path| text.is_absolute() && resolved(text) == wanted;
  let looked: Vec<(&Seen, bool, Option<Found>)> = seen
    .iter()
    .map(|process| {
      let named = process
        .data
        .iter()
        .chain(&process.data_directory)
        .any(|text| names(Path::new(text)));
      let found = match place(process.pid) {
        None | Some(Place::Here) => None,
        Some(Place::Unknown(reason)) => named.then(|| {
          Found::Unproven(format!(
            "the folder it runs in could not be read ({reason})"
          ))
        }),
        Some(Place::Elsewhere(None)) => named.then(|| {
          Found::Unproven(
            "the folder it runs in has no name any more, so no lock file could be read there"
              .into(),
          )
        }),
        Some(Place::Elsewhere(Some(folder))) => match lock(process.pid, &folder) {
          Ok(read) if read.lock.pid == process.pid && names(&read.lock.data) => {
            Some(if process.named_by_arguments {
              Found::Unproven(format!(
                "the lock file in the folder it runs in ({}) names it as the database of \
                 this path, but its program name was read from its arguments and not from \
                 the system (it runs through an emulator or an interpreter)",
                folder.display()
              ))
            } else {
              match start_agrees(read.lock.started, process.started) {
                Ok(()) => Found::Proven(folder, read),
                Err(why) => Found::Unproven(format!(
                  "the lock file in the folder it runs in ({}) has its number and this \
                   path, but it {why}",
                  folder.display()
                )),
              }
            })
          }
          read => named.then(|| {
            Found::Unproven(match read {
              Ok(read) if read.lock.pid == process.pid => format!(
                "the lock file in the folder it runs in ({}) gives {} as its data \
                 directory, not this path",
                folder.display(),
                read.lock.data.display()
              ),
              Ok(read) => format!(
                "the lock file in the folder it runs in ({}) names process {}, not this one",
                folder.display(),
                read.lock.pid
              ),
              Err(error) => format!(
                "the folder it runs in ({}) has no lock file that names it ({error})",
                folder.display()
              ),
            })
          }),
        },
      };
      (process, named, found)
    })
    .collect();
  let mut strays = Vec::new();
  for (process, _, found) in &looked {
    let helper = looked
      .iter()
      .any(|(other, named, found)| other.pid == process.parent && (*named || found.is_some()));
    match found {
      None => {}
      // What the lock file names is a main process, whatever started it.
      Some(Found::Proven(folder, evidence)) => strays.push(Stray::Elsewhere {
        process: (*process).clone(),
        folder: folder.clone(),
        evidence: evidence.clone(),
        helpers: seen
          .iter()
          .filter(|other| other.parent == process.pid)
          .cloned()
          .collect(),
      }),
      Some(Found::Unproven(_)) if helper => {}
      Some(Found::Unproven(reason)) => strays.push(Stray::Unproven {
        process: (*process).clone(),
        reason: reason.clone(),
      }),
    }
  }
  strays
}

/// The lock file in `folder`, which was read a moment ago as the folder process `pid`
/// runs in, and the identity of that folder.
///
/// The order matters, because a folder can be renamed, and another put at its name, at
/// any time: the folder is opened first, and its identity is that of the open folder;
/// the lock file is opened relative to the open folder (`openat`), never by a second
/// walk of the path; and only after the file was read is the working folder of the
/// process read and compared. So the text that is returned was read from the folder
/// whose identity is returned, and the process ran in that folder after the read.
#[cfg(unix)]
pub fn lock_where_it_runs(pid: u32, folder: &Path) -> Result<Evidence, String> {
  use std::io::Read;
  use std::os::fd::{AsRawFd, FromRawFd};
  use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
  let shown = folder.join("postmaster.pid");
  let opened = std::fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
    .open(folder)
    .map_err(|e| format!("{}: {e}", folder.display()))?;
  let at = opened
    .metadata()
    .map(|meta| folder_id(meta.dev(), meta.ino()))
    .map_err(|e| format!("{}: {e}", folder.display()))?;
  // SAFETY: the folder is open for the call, the name is a NUL-terminated constant, and
  // the new descriptor is owned by the `File` made from it right away.
  let file = unsafe {
    libc::openat(
      opened.as_raw_fd(),
      c"postmaster.pid".as_ptr(),
      libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
  };
  if file < 0 {
    let error = std::io::Error::last_os_error();
    return Err(format!("{}: {error}", shown.display()));
  }
  // SAFETY: `file` is a descriptor that was just opened and is owned by nothing else.
  let mut file = unsafe { std::fs::File::from_raw_fd(file) };
  let mut text = Vec::new();
  file
    .read_to_end(&mut text)
    .map_err(|e| format!("{}: {e}", shown.display()))?;
  match working_folder(pid)? {
    Some((running, _)) if running == at => Ok(Evidence {
      folder: at,
      lock: parse_pid(&String::from_utf8_lossy(&text))
        .map_err(|e| format!("{}: {e}", shown.display()))?,
    }),
    Some(_) => Err(format!(
      "{} is not the folder the process runs in any more",
      folder.display()
    )),
    None => Err("the process has ended".into()),
  }
}

/// Whether everything a signal to `process` rests on is still as it was when the look
/// took it (`folder`, `evidence`), read again now: the folder at that path is the same
/// folder, the lock file in it has the same first three lines, the process still runs in
/// it, that folder is not the one at `data`, and the process still runs the same
/// program. One exception to the last: where the system can no longer name the program
/// file because it was deleted while it runs (macOS), the name is not compared again;
/// the other four and the creation time still are. `Err`: what changed, as a clause;
/// nothing may be sent then.
///
/// The creation time of the process is read once more by [`signal_created`], directly
/// before the signal. What stays open: between that last reading and the `kill` call
/// (a few system calls, microseconds) the process could end and its number be given to
/// a new process. How likely that is depends on the range of numbers the system is set
/// to use (by default macOS up to 99998, Linux 4194304 on 64 bits; Linux can be set as
/// low as 301) and on how many processes are started in that moment. The gap is small,
/// and it is not zero.
#[cfg(unix)]
pub fn prove_again(
  data: &Path,
  process: &Seen,
  folder: &Path,
  evidence: &Evidence,
) -> Result<(), String> {
  use std::os::unix::fs::MetadataExt;
  let now = lock_where_it_runs(process.pid, folder)?;
  if &now != evidence {
    return Err(format!(
      "the folder it runs in ({}) or the lock file there is not what it was a moment ago",
      folder.display()
    ));
  }
  if std::fs::metadata(data).is_ok_and(|meta| folder_id(meta.dev(), meta.ino()) == now.folder) {
    return Err("the folder it runs in is the folder at this data path now".into());
  }
  match holder(process.pid) {
    Holder::Program(program) if program == process.program => Ok(()),
    // The program file was deleted while it runs (macOS): its name cannot be read again.
    Holder::Gone(_) => Ok(()),
    Holder::Program(program) => Err(format!(
      "it runs another program now ({})",
      program.display()
    )),
    Holder::Free | Holder::Ended => Err("the process has ended".into()),
    Holder::Unreadable(error) => Err(error),
  }
}

/// [`place`] for the look before a start, when the data folder may not be there yet (a
/// new install): no process runs in a folder that does not exist, so a working folder
/// that was read is another folder. `None`: no such process.
#[cfg(unix)]
pub fn place_before_start(pid: u32, data: &Path) -> Option<Place> {
  use std::os::unix::fs::MetadataExt;
  let working = match working_folder(pid) {
    Ok(None) => return None,
    other => other,
  };
  Some(match (std::fs::metadata(data), working) {
    (Err(error), Ok(Some((_, path)))) if error.kind() == std::io::ErrorKind::NotFound => {
      Place::Elsewhere(path)
    }
    (folder, working) => placed(
      folder
        .map(|meta| folder_id(meta.dev(), meta.ino()))
        .map_err(|e| format!("{}: {e}", data.display())),
      working,
    ),
  })
}

/// Every live process of this user whose program is called like the PostgreSQL server.
/// A process that cannot be read (it ended meanwhile, or it is another user's) is left
/// out: nothing is known about it and nothing is done to it.
#[cfg(target_os = "macos")]
pub fn postgres_processes() -> Vec<Seen> {
  // SAFETY: geteuid takes no arguments and cannot fail.
  let user = unsafe { libc::geteuid() };
  // SAFETY: a null buffer of size 0 only asks how many process numbers there are.
  let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
  if count <= 0 {
    return Vec::new();
  }
  // Room for processes that start between the two calls.
  let mut pids = vec![0 as libc::pid_t; count as usize + 256];
  // SAFETY: the buffer is writable for the byte length given; nothing is retained.
  let filled = unsafe {
    libc::proc_listallpids(
      pids.as_mut_ptr().cast(),
      (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
    )
  };
  pids.truncate(filled.max(0) as usize);
  pids
    .into_iter()
    .filter(|pid| *pid > 1)
    .filter_map(|pid| {
      let pid = pid as u32;
      let info = bsd_info(pid).ok()?;
      if info.pbi_uid != user || info.pbi_status == libc::SZOMB {
        return None;
      }
      // The name first: the argument record is read for PostgreSQL names only.
      let program = match program_file(pid) {
        Ok(program) => Some(program),
        Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
        Err(_) => return None,
      };
      if program
        .as_deref()
        .is_some_and(|program| !postgres_like(program))
      {
        return None;
      }
      let started = started(pid).ok()?;
      let program = program.unwrap_or_else(|| started.program.clone());
      postgres_like(&program).then(|| Seen {
        pid,
        parent: info.pbi_ppid,
        program,
        created: format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec),
        data_directory: data_directory_texts(&started.args),
        data: data_text(&started.args, started.pgdata),
        started: Some(info.pbi_start_tvsec as i64),
        named_by_arguments: false,
      })
    })
    .collect()
}

#[cfg(target_os = "linux")]
pub fn postgres_processes() -> Vec<Seen> {
  // SAFETY: geteuid takes no arguments and cannot fail.
  linux_processes(Path::new("/proc"), unsafe { libc::geteuid() })
}

/// [`postgres_processes`] from `proc` (`/proc`; a made-up tree in the tests), for the
/// processes of user `user`.
#[cfg(any(target_os = "linux", all(unix, test)))]
pub(crate) fn linux_processes(proc: &Path, user: u32) -> Vec<Seen> {
  use std::os::unix::ffi::OsStrExt;
  use std::os::unix::fs::MetadataExt;
  let boot = std::fs::read_to_string(proc.join("sys/kernel/random/boot_id")).unwrap_or_default();
  let Ok(entries) = std::fs::read_dir(proc) else {
    return Vec::new();
  };
  entries
    .filter_map(Result::ok)
    .filter_map(|entry| {
      let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
      let root = entry.path();
      if pid <= 1 || std::fs::metadata(&root).ok()?.uid() != user {
        return None;
      }
      let stat = std::fs::read_to_string(root.join("stat")).ok()?;
      // `pid (name) state parent …`; the name may itself contain spaces and parentheses.
      let fields: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
      if matches!(fields.first(), Some(&"Z") | Some(&"X") | None) {
        return None;
      }
      let (program, named_by_arguments) = linux_program(&root).ok()?;
      if !postgres_like(&program) {
        return None;
      }
      let strings = |file: &str| -> Option<Vec<String>> {
        Some(
          std::fs::read(root.join(file))
            .ok()?
            .split(|b| *b == 0)
            .filter(|b| !b.is_empty())
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect(),
        )
      };
      let args = strings("cmdline")?;
      // Under emulation the arguments begin with the emulator; the data directory text
      // is looked for after the program itself.
      let from = args
        .iter()
        .position(|arg| Path::new(arg) == program)
        .unwrap_or(0);
      let pgdata = std::fs::read(root.join("environ")).ok().and_then(|bytes| {
        bytes
          .split(|b| *b == 0)
          .filter_map(|entry| entry.strip_prefix(b"PGDATA="))
          .map(|value| {
            std::ffi::OsStr::from_bytes(value)
              .to_string_lossy()
              .into_owned()
          })
          .next()
      });
      Some(Seen {
        pid,
        parent: fields.get(1)?.parse().ok()?,
        program,
        // As `identity` gives it.
        created: format!("{}:{}", boot.trim(), fields.get(19)?),
        data_directory: data_directory_texts(&args[from..]),
        data: data_text(&args[from..], pgdata),
        started: linux_started(proc, fields.get(19)?),
        named_by_arguments,
      })
    })
    .collect()
}

/// When a Linux process was created, in seconds since 1970: the time of the boot
/// (`btime` in `<proc>/stat`) plus its start in clock ticks after the boot (`ticks`,
/// field 22 of its `stat`). `None`: one of them could not be read.
#[cfg(any(target_os = "linux", all(unix, test)))]
fn linux_started(proc: &Path, ticks: &str) -> Option<i64> {
  let boot: i64 = std::fs::read_to_string(proc.join("stat"))
    .ok()?
    .lines()
    .find_map(|line| line.strip_prefix("btime "))?
    .trim()
    .parse()
    .ok()?;
  // SAFETY: sysconf takes a constant and has no side effect.
  let per_second = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
  (per_second > 0).then_some(())?;
  Some(boot + ticks.parse::<i64>().ok()? / per_second as i64)
}

/// Whether `pid` is still the process that was created at `created`. A process that
/// cannot be looked at counts as still there.
#[cfg(unix)]
pub fn still(pid: u32, created: &str) -> bool {
  match identity(pid) {
    Ok(None) => false,
    Ok(Some(now)) => now.created == created,
    Err(_) => true,
  }
}

/// Send `signal` to the process `pid` that was created at `created`, and to no other:
/// its creation time is read again right before. `Ok(false)`: it is gone.
#[cfg(unix)]
pub fn signal_created(pid: u32, created: &str, signal: libc::c_int) -> Result<bool, String> {
  match identity(pid)? {
    Some(now) if now.created == created => {
      // SAFETY: the PID was just proven to be the process that was looked at.
      if unsafe { libc::kill(pid as i32, signal) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
          return Ok(false);
        }
        return Err(format!("Could not signal process {pid}: {error}"));
      }
      Ok(true)
    }
    _ => Ok(false),
  }
}

#[cfg(target_os = "linux")]
pub fn identity(pid: u32) -> Result<Option<Identity>, String> {
  linux_identity(Path::new("/proc"), pid)
}

/// Linux, from `proc` (`/proc`; a made-up tree in the tests): the identity of the live
/// process `pid`. `Ok(None)`: no live process has this number.
///
/// The entry of a process is several files that are read one after another, and a
/// process can end while that is done. It ends in steps. First the system marks it as
/// ending (the `PF_EXITING` bit, 4, in the flags of its `stat`). Then it gives up its
/// memory, and with it the `exe` link: from then on the link answers "no such file",
/// while `stat` still shows a running state. Then `stat` shows state `Z`, and when its
/// parent has collected it the whole entry is gone. A process that takes long to give
/// up its memory (a large one) stays in the second step for that long.
///
/// So when the link or the arguments answer "no such file" or "no such process", `stat`
/// is read once more, and a process that is gone, ended or marked as ending is
/// `Ok(None)`: it runs no program and never will again. Before, that moment was given
/// back as an error, "the process could not be inspected". A process without the link
/// that is not ending (a task of the kernel) stays an error: what it is, is not known.
///
/// Compiled for Linux, and for the tests of the other Unix systems.
#[cfg(any(target_os = "linux", all(unix, test)))]
pub(crate) fn linux_identity(proc: &Path, pid: u32) -> Result<Option<Identity>, String> {
  /// `PF_EXITING` of the kernel: the process is being ended.
  const ENDING: u64 = 0x4;
  let root = proc.join(pid.to_string());
  let absent = |error: &std::io::Error| {
    error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
  };
  let cannot = |error: std::io::Error| format!("Cannot inspect process {pid}: {error}");
  // The fields of `stat` after the name: the state first, the flags seventh, the start
  // twentieth. The name may itself contain spaces and parentheses.
  let fields = |stat: &str| -> Option<Vec<String>> {
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_string).collect())
  };
  let ended =
    |fields: &[String]| matches!(fields.first().map(String::as_str), Some("Z") | Some("X"));
  let stat = match std::fs::read_to_string(root.join("stat")) {
    Ok(stat) => fields(&stat).ok_or("Invalid /proc stat")?,
    Err(error) if absent(&error) => return Ok(None),
    Err(error) => return Err(cannot(error)),
  };
  if ended(&stat) {
    return Ok(None);
  }
  let started = stat.get(19).ok_or("Missing process creation identity")?;
  let boot = std::fs::read_to_string(proc.join("sys/kernel/random/boot_id"))
    .map_err(|e| format!("Cannot inspect process boot identity: {e}"))?;
  let created = format!("{}:{started}", boot.trim());
  // A file of the entry is not there: has the process ended, or is it ending?
  let ending = || match std::fs::read_to_string(root.join("stat")) {
    Err(error) => absent(&error),
    Ok(stat) => fields(&stat).is_some_and(|now| {
      ended(&now)
        || now
          .get(6)
          .and_then(|flags| flags.parse::<u64>().ok())
          .is_some_and(|flags| flags & ENDING != 0)
    }),
  };
  let image = match std::fs::read_link(root.join("exe")) {
    Ok(image) => image,
    Err(error) if absent(&error) && ending() => return Ok(None),
    Err(error) => return Err(cannot(error)),
  };
  let bytes = match std::fs::read(root.join("cmdline")) {
    Ok(bytes) => bytes,
    Err(error) if absent(&error) && ending() => return Ok(None),
    Err(error) => return Err(cannot(error)),
  };
  let args = bytes
    .split(|b| *b == 0)
    .filter(|b| !b.is_empty())
    .map(|b| String::from_utf8_lossy(b).into_owned())
    .collect();
  Ok(Some(Identity {
    pid,
    image,
    created,
    args,
  }))
}

/// What macOS keeps of the way a process was started: the path it was started with, its
/// arguments, and the `PGDATA` of its environment. A PostgreSQL main process leaves this
/// record as it was; its helper processes overwrite theirs with a title.
#[cfg(target_os = "macos")]
struct Started {
  program: PathBuf,
  args: Vec<String>,
  pgdata: Option<String>,
}

#[cfg(target_os = "macos")]
fn started(pid: u32) -> Result<Started, String> {
  // KERN_PROCARGS2 returns argc, executable, null padding, then exactly argc
  // null-delimited arguments, then the environment. Unlike ps text it preserves Unicode
  // and spaces.
  let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as i32];
  let mut bytes = vec![0u8; 1024 * 1024];
  let mut len = bytes.len();
  // SAFETY: valid mib and writable output buffers; sysctl receives their lengths.
  let ok = unsafe {
    libc::sysctl(
      mib.as_mut_ptr(),
      3,
      bytes.as_mut_ptr().cast(),
      &mut len,
      std::ptr::null_mut(),
      0,
    )
  };
  if ok != 0 {
    return Err(format!(
      "Cannot inspect process arguments: {}",
      std::io::Error::last_os_error()
    ));
  }
  bytes.truncate(len);
  if len < 4 {
    return Err("Invalid process argument record".into());
  }
  let argc = i32::from_ne_bytes(
    bytes[..4]
      .try_into()
      .map_err(|e: std::array::TryFromSliceError| e.to_string())?,
  ) as usize;
  let started_as = 4
    + bytes[4..]
      .iter()
      .position(|b| *b == 0)
      .ok_or("Missing executable terminator")?;
  use std::os::unix::ffi::OsStringExt;
  let program = PathBuf::from(std::ffi::OsString::from_vec(bytes[4..started_as].to_vec()));
  let mut at = started_as + 1;
  while at < len && bytes[at] == 0 {
    at += 1;
  }
  let mut strings = bytes[at..].split(|b| *b == 0);
  let args: Vec<String> = strings
    .by_ref()
    .take(argc)
    .map(|b| String::from_utf8_lossy(b).into_owned())
    .collect();
  let pgdata = strings
    .filter_map(|entry| entry.strip_prefix(b"PGDATA="))
    .map(|value| String::from_utf8_lossy(value).into_owned())
    .next();
  Ok(Started {
    program,
    args,
    pgdata,
  })
}

/// The system's own record of process `pid` (owner, parent, state, start time).
#[cfg(target_os = "macos")]
fn bsd_info(pid: u32) -> std::io::Result<libc::proc_bsdinfo> {
  // SAFETY: a plain C struct of integers and byte arrays; all zeroes is a valid value.
  let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
  // SAFETY: writable POD buffer with its exact size; proc_pidinfo retains nothing.
  let read = unsafe {
    libc::proc_pidinfo(
      pid as i32,
      libc::PROC_PIDTBSDINFO,
      0,
      (&mut info as *mut libc::proc_bsdinfo).cast(),
      std::mem::size_of_val(&info) as i32,
    )
  };
  if read != std::mem::size_of_val(&info) as i32 {
    return Err(std::io::Error::last_os_error());
  }
  Ok(info)
}

#[cfg(target_os = "macos")]
pub fn identity(pid: u32) -> Result<Option<Identity>, String> {
  let program = match program_file(pid) {
    Ok(program) => Some(program),
    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => return Ok(None),
    // The file is gone but the process runs: the path it was started with (below).
    Err(e) if e.raw_os_error() == Some(libc::ENOENT) => None,
    Err(e) => return Err(format!("Cannot inspect process {pid}: {e}")),
  };
  let started = started(pid)?;
  let image = program.unwrap_or(started.program);
  // Native microsecond creation identity avoids both localized ps formatting
  // and PID reuse within the same second.
  let info = bsd_info(pid).map_err(|e| format!("Cannot inspect process creation identity: {e}"))?;
  if info.pbi_status == libc::SZOMB {
    return Ok(None);
  }
  let created = format!("{}:{}", info.pbi_start_tvsec, info.pbi_start_tvusec);
  Ok(Some(Identity {
    pid,
    image,
    created,
    args: started.args,
  }))
}

#[cfg(windows)]
pub fn identity(pid: u32) -> Result<Option<Identity>, String> {
  use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
  use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_QUERY_LIMITED_INFORMATION,
  };
  // SAFETY: each handle is closed; outputs are initialized buffers/POD.
  unsafe {
    let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if handle.is_null() {
      let error = std::io::Error::last_os_error();
      if error.raw_os_error() == Some(87) {
        return Ok(None);
      }
      return Err(format!("Cannot inspect process {pid}: {error}"));
    }
    let result = (|| {
      let mut code = 0;
      if GetExitCodeProcess(handle, &mut code) == 0 {
        return Err(std::io::Error::last_os_error().to_string());
      }
      if code != 259 {
        return Ok(None);
      }
      let mut buffer = vec![0u16; 32768];
      let mut size = buffer.len() as u32;
      if QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) == 0 {
        return Err(std::io::Error::last_os_error().to_string());
      }
      let image = PathBuf::from(String::from_utf16_lossy(&buffer[..size as usize]));
      let mut times: [FILETIME; 4] = std::mem::zeroed();
      if GetProcessTimes(
        handle,
        times.as_mut_ptr(),
        times.as_mut_ptr().add(1),
        times.as_mut_ptr().add(2),
        times.as_mut_ptr().add(3),
      ) == 0
      {
        return Err(std::io::Error::last_os_error().to_string());
      }
      let created = format!("{}:{}", times[0].dwHighDateTime, times[0].dwLowDateTime);
      // CIM returns the wide command line. It is used only for exact -D proof;
      // image and creation identity come from native handle queries above.
      let script = format!("[Console]::OutputEncoding=[Text.UTF8Encoding]::new(); (Get-CimInstance Win32_Process -Filter 'ProcessId={pid}' -ErrorAction Stop).CommandLine");
      let out = process_query(&script)?;
      if !out.status.success() {
        return Err("Cannot prove PostgreSQL data argument (process query failed)".into());
      }
      let args = windows_args(String::from_utf8_lossy(&out.stdout).trim())?;
      Ok(Some(Identity {
        pid,
        image,
        created,
        args,
      }))
    })();
    CloseHandle(handle);
    result
  }
}

#[cfg(windows)]
fn process_query(script: &str) -> Result<std::process::Output, String> {
  use std::os::windows::process::CommandExt;
  let mut child = std::process::Command::new("powershell.exe")
    .args(["-NoProfile", "-NonInteractive", "-Command", script])
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .creation_flags(0x0800_0000)
    .spawn()
    .map_err(|e| e.to_string())?;
  let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
  while child.try_wait().map_err(|e| e.to_string())?.is_none() {
    if std::time::Instant::now() >= deadline {
      let _ = child.kill();
      let _ = child.wait();
      return Err("Process argument query timed out; no unverified process was stopped".into());
    }
    std::thread::sleep(std::time::Duration::from_millis(50));
  }
  child.wait_with_output().map_err(|e| e.to_string())
}

#[cfg(any(windows, test))]
fn windows_args(command: &str) -> Result<Vec<String>, String> {
  // PostgreSQL's launch args contain no escaped quotes/backslashes before quotes;
  // reject those ambiguous forms rather than guessing at ownership.
  let mut quoted = false;
  let mut part = String::new();
  let mut args = Vec::new();
  for c in command.chars() {
    if c == '"' {
      if part.ends_with('\\') {
        return Err("Ambiguous process command line".into());
      }
      quoted = !quoted;
    } else if c.is_whitespace() && !quoted {
      if !part.is_empty() {
        args.push(std::mem::take(&mut part));
      }
    } else {
      part.push(c);
    }
  }
  if quoted {
    return Err("Unclosed process command line quote".into());
  }
  if !part.is_empty() {
    args.push(part);
  }
  Ok(args)
}

pub fn kill_identified(expected: &Identity) -> Result<(), String> {
  if identity(expected.pid)?.as_ref() != Some(expected) {
    return Err("Process identity changed; nothing was killed".into());
  }
  #[cfg(unix)]
  {
    // SAFETY: the PID was just checked for image, arguments and creation identity.
    if unsafe { libc::kill(expected.pid as i32, libc::SIGKILL) } != 0 {
      return Err(std::io::Error::last_os_error().to_string());
    }
  }
  #[cfg(windows)]
  {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
      GetProcessTimes, OpenProcess, TerminateProcess, WaitForSingleObject,
      PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
    };
    // SAFETY: verify creation on the SAME live handle used for termination.
    unsafe {
      let handle = OpenProcess(
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
        0,
        expected.pid,
      );
      if handle.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
      }
      let mut times: [FILETIME; 4] = std::mem::zeroed();
      let ok = GetProcessTimes(
        handle,
        times.as_mut_ptr(),
        times.as_mut_ptr().add(1),
        times.as_mut_ptr().add(2),
        times.as_mut_ptr().add(3),
      ) != 0
        && format!("{}:{}", times[0].dwHighDateTime, times[0].dwLowDateTime) == expected.created;
      if !ok {
        CloseHandle(handle);
        return Err("Process identity changed; nothing was killed".into());
      }
      let result = TerminateProcess(handle, 1) != 0 && WaitForSingleObject(handle, 5000) == 0;
      CloseHandle(handle);
      if !result {
        return Err("Identified postmaster termination failed".into());
      }
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  /// Run 38172246950 failed here on Windows: the one accepted example was `/tmp/…`,
  /// which names no drive and so is rightly refused there. Both systems' rules are now
  /// tested on every system, and the parser of the running system with a path of it.
  #[test]
  fn pid_parsing_rejects_unsafe_records() {
    for windows in [false, true] {
      for input in [
        "",
        "1\n/tmp/data",
        "1\nC:/data",
        "0\nC:/data",
        "-23\nC:/data",
        "garbage\n/tmp/data",
        "23",
        "23\n",
        "23\r\n",
        "23\nrelative",
        "23\nrelative/data",
        "23\n./data",
        " 23\n/tmp/data",
      ] {
        assert!(parse_pid_as(input, windows).is_err(), "{input:?} {windows}");
      }
    }
    let unix = parse_pid_as("23\n/tmp/a b é漢字\n0\n5432", false).unwrap();
    assert_eq!((unix.pid, unix.data), (23, PathBuf::from("/tmp/a b é漢字")));
    // Unix knows no drive and no share: these are relative names there.
    for input in ["23\nC:/data", "23\nC:\\data", "23\n\\\\server\\share\\data"] {
      assert!(parse_pid_as(input, false).is_err(), "{input:?}");
    }
    // The running system, with a full path of it.
    let here = if cfg!(windows) {
      "C:/Users/a b é漢字/data"
    } else {
      "/tmp/a b é漢字"
    };
    let parsed = parse_pid(&format!("23\n{here}\n0\n5432")).unwrap();
    assert_eq!((parsed.pid, parsed.data.as_path()), (23, Path::new(here)));
    assert!(parsed.data.is_absolute());
  }

  /// Windows: exactly the forms PostgreSQL 18 writes (forward slashes, after its
  /// `canonicalize_path`), the same with backslashes, and `\r\n` line ends.
  #[test]
  fn pid_parsing_takes_what_postgresql_writes_on_windows() {
    for data in [
      "C:/Users/José 漢字/AppData/Roaming/Komiq Server/server/postgres/18/data",
      "c:/data",
      "D:\\Komiq Server\\server\\postgres\\18\\data",
      "//server/share/Komiq Server/data",
      "\\\\server\\share\\Komiq Server\\data",
      "//server/share",
      "//?/C:/Users/José/data",
      "\\\\?\\C:\\Users\\José\\data",
      "\\\\?\\UNC\\server\\share\\data",
    ] {
      for end in ["\n", "\r\n"] {
        let text = format!("4242{end}{data}{end}1760000000{end}5432{end}{end}localhost{end}");
        let parsed = parse_pid_as(&text, true).unwrap_or_else(|e| panic!("{data:?}: {e}"));
        assert_eq!(parsed.pid, 4242);
        // The text is kept as PostgreSQL wrote it, without the line end.
        assert_eq!(parsed.data, PathBuf::from(data), "{data:?} {end:?}");
      }
    }
    for data in [
      // A root without a drive: which drive depends on the reader.
      "/data",
      "\\data",
      "/Users/José/data",
      // Relative to the working directory of a drive.
      "C:data",
      "C:",
      // No letter, or more than one, before the colon.
      "1:/data",
      "é:/data",
      "CD:/data",
      ":/data",
      // No server, or no share.
      "//",
      "///share/data",
      "//server",
      "//server/",
      "\\\\server\\",
      "relative\\data",
      "data",
    ] {
      for end in ["\n", "\r\n"] {
        assert!(
          parse_pid_as(&format!("4242{end}{data}{end}1760000000{end}"), true).is_err(),
          "{data:?}"
        );
      }
    }
  }

  /// The text rule and the standard library of the running system agree for a drive
  /// path and for a path without a drive, so the folder compared later (`owns`) is the
  /// folder the rule let through.
  #[test]
  fn the_data_folder_rule_agrees_with_the_running_system() {
    let cases: &[&str] = if cfg!(windows) {
      &[
        "C:/a b/data",
        "C:\\a b\\data",
        "/data",
        "\\data",
        "C:data",
        "data",
      ]
    } else {
      &["/a b/data", "data", "./data", "C:/data"]
    };
    for case in cases {
      assert_eq!(
        names_one_folder(case, cfg!(windows)),
        Path::new(case).is_absolute(),
        "{case:?}"
      );
    }
  }
  #[test]
  fn windows_command_preserves_spaced_data_argument() {
    assert_eq!(
      windows_args(r#""C:\Program Files\postgres.exe" -D "C:\pg data\data" -p 1234"#).unwrap(),
      vec![
        r"C:\Program Files\postgres.exe",
        "-D",
        r"C:\pg data\data",
        "-p",
        "1234"
      ]
    );
    assert!(windows_args("postgres -D \"bad").is_err());
  }
}
