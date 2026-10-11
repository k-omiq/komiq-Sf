//! Wide paths stay authoritative. Windows tools get ASCII junction aliases of the data
//! folder and of the runtime folder when their paths are not ASCII.
use std::fs;
use std::path::{Path, PathBuf};

/// Create `dir` when it is missing and keep it private to this user (Unix mode 0700).
///
/// The server refuses to convert a SQLite file whose folder is writable by the group or
/// by others, or belongs to another user (`validate_directory` in
/// apps/server/src/pg_import/startup.rs). A session with umask 002 would otherwise
/// create the folder 0775. A folder this user owns is tightened at every start. A link is
/// never followed and a folder of another owner is never changed: both come back as a
/// sentence for the owner of the machine. Windows: only creates the folder.
pub fn private_dir(dir: &Path) -> Result<Option<String>, String> {
  #[cfg(unix)]
  {
    // SAFETY: geteuid takes no arguments and cannot fail.
    private_dir_as(dir, unsafe { libc::geteuid() })
  }
  #[cfg(not(unix))]
  {
    fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    Ok(None)
  }
}

/// [`private_dir`] for the user `uid` (injected by the tests).
#[cfg(unix)]
pub(crate) fn private_dir_as(dir: &Path, uid: u32) -> Result<Option<String>, String> {
  use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
  const UNCONVERTED: &str = "An old database in it is not converted until this is put right";
  if fs::symlink_metadata(dir).is_err() {
    fs::DirBuilder::new()
      .recursive(true)
      .mode(0o700)
      .create(dir)
      .map_err(|e| e.to_string())?;
  }
  let own = fs::symlink_metadata(dir).map_err(|e| e.to_string())?;
  if own.file_type().is_symlink() {
    // Read through the link, change nothing behind it.
    let target = fs::metadata(dir).map_err(|e| e.to_string())?;
    if target.uid() == uid && target.mode() & 0o022 == 0 {
      return Ok(None);
    }
    return Ok(Some(format!(
      "The data folder {} is a link, so its permissions were left alone, and the folder it \
       points to is writable by other users or belongs to another user. {UNCONVERTED}: \
       make that folder yours and private (chmod 700).",
      dir.display()
    )));
  }
  // The handle is the folder itself: a link put at this name after the check above is
  // refused by O_NOFOLLOW, and the mode is changed through the handle, not the name.
  let handle = fs::OpenOptions::new()
    .read(true)
    .custom_flags(libc::O_NOFOLLOW | libc::O_DIRECTORY)
    .open(dir)
    .map_err(|e| e.to_string())?;
  let meta = handle.metadata().map_err(|e| e.to_string())?;
  if meta.uid() != uid {
    return Ok(Some(format!(
      "The data folder {} belongs to another user (id {}), so its permissions were left \
       alone. {UNCONVERTED}.",
      dir.display(),
      meta.uid()
    )));
  }
  if meta.mode() & 0o077 != 0 {
    handle
      .set_permissions(fs::Permissions::from_mode(0o700))
      .map_err(|e| e.to_string())?;
    log::info!(
      "made the data folder {} private (was mode {:o})",
      dir.display(),
      meta.mode() & 0o7777
    );
  }
  Ok(None)
}

/// The plain form of a Windows path in the `\\?\` (verbatim) form, when the plain form
/// names the same file: `\\?\C:\x` → `C:\x`, `\\?\UNC\server\share\x` →
/// `\\server\share\x`. `None`: keep the path as it is (not a verbatim path, or one that
/// only the verbatim form can name).
///
/// `canonicalize` returns the verbatim form on Windows. PostgreSQL's programs and
/// PowerShell do not take it: they split paths at `/` and `\`, build other paths from
/// the one they were started with, and compare path texts. The plain form says the same
/// when the path is a drive or a network share, is shorter than the old 260 character
/// limit, and has no part that only the verbatim form can spell (a name that ends in a
/// dot or a space, a device name like `NUL`, a character Windows does not allow in
/// names, `.` or `..`).
pub fn plain_form(path: &str) -> Option<String> {
  let rest = path.strip_prefix(r"\\?\")?;
  let (plain, parts) = if let Some(share) = rest.strip_prefix(r"UNC\") {
    // server and share name must both be there.
    let mut names = share.split('\\');
    let named = |name: Option<&str>| name.is_some_and(|name| !name.is_empty());
    if !named(names.next()) || !named(names.next()) {
      return None;
    }
    (format!(r"\\{share}"), share)
  } else {
    let bytes = rest.as_bytes();
    let drive =
      bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'\\';
    if !drive {
      return None; // `\\?\Volume{…}\`, `\\?\GLOBALROOT\…`: no plain form
    }
    (rest.to_string(), &rest[3..])
  };
  if plain.encode_utf16().count() >= 260 {
    return None;
  }
  const DEVICES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9", "CONIN$",
    "CONOUT$",
  ];
  for part in parts.split('\\') {
    if part.is_empty() {
      continue; // the end of `C:\`, or of a path with a closing separator
    }
    let stem = part.split('.').next().unwrap_or(part).trim_end_matches(' ');
    if part == "."
      || part == ".."
      || part.ends_with('.')
      || part.ends_with(' ')
      || part
        .chars()
        .any(|c| c.is_control() || r#"<>:"/|?*"#.contains(c))
      || DEVICES
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
    {
      return None;
    }
  }
  Some(plain)
}

/// A path as it may be given to a child process, on its command line or in its
/// environment: on Windows the plain form of a verbatim path ([`plain_form`]); on every
/// other system the path itself.
pub fn tool_path(path: PathBuf) -> PathBuf {
  if cfg!(windows) {
    if let Some(plain) = path.to_str().and_then(plain_form) {
      return PathBuf::from(plain);
    }
  }
  path
}

#[cfg(any(windows, test))]
pub fn needs_junction(windows: bool, path: &Path) -> bool {
  windows && !path.to_string_lossy().is_ascii()
}

/// Where the ASCII aliases (directory junctions) of one install go on Windows.
#[cfg(any(windows, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AliasHome {
  /// A folder that belongs to this install alone. The aliases have fixed names in it, so
  /// an app that was force-quit leaves at most one of each and the next start reuses or
  /// replaces it.
  Own(PathBuf),
  /// A folder other users can write to (the system's temporary folders). The alias
  /// folder gets a fresh name nobody can guess; the data folder remembers it, so the
  /// next start removes what a force-quit app left before it makes a new one.
  Shared(PathBuf),
}

/// The places to try, in order. An alias only helps when its own path is ASCII, so a
/// place whose path is not ASCII is left out. That is why the app's own data folder, the
/// first choice, cannot hold the alias for a user whose profile path is not ASCII: there
/// the system's temporary folder is used.
#[cfg(any(windows, test))]
pub(crate) fn alias_homes(
  explicit: Option<PathBuf>,
  app_data: &Path,
  shared: Vec<PathBuf>,
) -> Vec<AliasHome> {
  let usable = |path: &Path| path.is_absolute() && path.to_string_lossy().is_ascii();
  let mut homes = Vec::new();
  if let Some(base) = explicit.filter(|base| usable(base)) {
    // The owner of the machine chose it; one fixed folder per data folder inside it.
    homes.push(AliasHome::Own(
      base.join(format!("komiq-pg-{}", fixed_suffix(app_data))),
    ));
  }
  if usable(app_data) {
    homes.push(AliasHome::Own(app_data.join("pg-alias")));
  }
  for base in shared {
    if usable(&base) && !homes.contains(&AliasHome::Shared(base.clone())) {
      homes.push(AliasHome::Shared(base));
    }
  }
  homes
}

/// Sixteen hex digits that are the same at every start of one install.
#[cfg(any(windows, test))]
fn fixed_suffix(app_data: &Path) -> String {
  use sha2::{Digest, Sha256};
  let digest = Sha256::digest(app_data.to_string_lossy().as_bytes());
  digest[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The file in the data folder that remembers an alias folder made in a shared place.
#[cfg(any(windows, test))]
const ALIAS_MEMO: &str = "ascii-alias";

/// Remove the alias folder a force-quit app left in a shared place: the two junctions
/// and their folder, never anything behind them (`remove_dir` does not follow a junction
/// and does not remove a folder that has content).
#[cfg(any(windows, test))]
pub(crate) fn forget_remembered_alias(memo: &Path) {
  let Ok(text) = fs::read_to_string(memo) else {
    return;
  };
  let parent = PathBuf::from(text.trim());
  let ours = parent.is_absolute()
    && parent
      .file_name()
      .is_some_and(|name| name.to_string_lossy().starts_with("komiq-pg-"));
  if ours {
    let _ = fs::remove_dir(parent.join("server"));
    let _ = fs::remove_dir(parent.join("runtime"));
    let _ = fs::remove_dir(&parent);
  }
  let _ = fs::remove_file(memo);
}

/// Reserve the alias folder `parent` in a shared place: first write its name into `memo`
/// (the intent), then create the folder, which must be new so that it is this user's own.
/// An app that is killed between the two steps leaves a memo that names a folder that
/// does not exist, and the next start only removes the memo. In the other order it would
/// leave a folder that nothing remembers. When the folder cannot be made, the memo is
/// taken back and `false` is returned. `create` is `fs::create_dir` (a test gives another).
#[cfg(any(windows, test))]
pub(crate) fn reserve_alias(
  memo: &Path,
  parent: &Path,
  create: impl FnOnce(&Path) -> std::io::Result<()>,
) -> bool {
  if fs::write(memo, parent.to_string_lossy().as_bytes()).is_err() {
    return false;
  }
  if create(parent).is_err() {
    // Nothing was made. (A folder of that name that was already there is not ours and
    // stays: the name has 64 random hex digits, so this is a failure, not a meeting.)
    let _ = fs::remove_file(memo);
    return false;
  }
  // Confirm: the memo still names the folder that now exists.
  fs::read_to_string(memo).is_ok_and(|text| Path::new(text.trim()) == parent) && parent.is_dir()
}

#[cfg(windows)]
fn new_dir(dir: &Path) -> std::io::Result<()> {
  fs::create_dir(dir)
}

/// Make `link` a directory junction to `target`, or keep it when it already is one.
#[cfg(windows)]
async fn junction(link: &Path, target: &Path) -> bool {
  let resolves = |link: &Path| {
    let there = link.canonicalize().ok();
    there.is_some() && there == target.canonicalize().ok()
  };
  if resolves(link) {
    return true; // left by a force-quit app, and still right
  }
  // A junction to another place, or an empty folder: removed itself, never followed.
  let _ = fs::remove_dir(link);
  let quote = |p: &Path| {
    use std::os::windows::ffi::OsStrExt;
    let units = p
      .as_os_str()
      .encode_wide()
      .map(|u| u.to_string())
      .collect::<Vec<_>>()
      .join(",");
    format!("([String]::new([char[]]@({units})))")
  };
  // PowerShell gets the plain forms: it does not take a `\\?\` path for a junction.
  let script = format!(
    "$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path {} -Target {} | Out-Null",
    quote(&tool_path(link.to_path_buf())),
    quote(&tool_path(target.to_path_buf()))
  );
  let mut cmd = tokio::process::Command::new("powershell.exe");
  cmd
    .args(["-NoProfile", "-NonInteractive", "-Command", &script])
    .kill_on_drop(true)
    .creation_flags(0x0800_0000);
  let result = tokio::time::timeout(std::time::Duration::from_secs(15), cmd.output()).await;
  result.is_ok_and(|r| r.is_ok_and(|out| out.status.success())) && resolves(link)
}

pub struct ToolPaths {
  pub server: PathBuf,
  pub real_server: PathBuf,
  /// The runtime tree as the tools are started from: the real path, or on Windows its
  /// ASCII alias. PostgreSQL's programs find their own files from the path they were
  /// started with, which they read in the system's code page.
  pub runtime: PathBuf,
  /// The alias folder this value made and removes again.
  junction: Option<PathBuf>,
  /// The file that remembers `junction` when it lies in a shared place.
  memo: Option<PathBuf>,
}
impl ToolPaths {
  pub fn borrowed(&self) -> Self {
    Self {
      server: self.server.clone(),
      real_server: self.real_server.clone(),
      runtime: self.runtime.clone(),
      junction: None,
      memo: None,
    }
  }
  pub async fn new(real_server: &Path, real_runtime: &Path) -> Result<Self, String> {
    // What the tools are given: never a `\\?\` path (Windows; see `tool_path`).
    let tool_server = tool_path(real_server.to_path_buf());
    let tool_runtime = tool_path(real_runtime.to_path_buf());
    #[cfg(windows)]
    {
      let memo = real_server.join("postgres").join(ALIAS_MEMO);
      forget_remembered_alias(&memo);
      let need_server = needs_junction(true, real_server);
      let need_runtime = needs_junction(true, real_runtime);
      if need_server || need_runtime {
        let app_data = real_server.parent().unwrap_or(real_server);
        let mut shared = vec![std::env::temp_dir()];
        if let Some(windir) = std::env::var_os("SystemRoot") {
          shared.push(PathBuf::from(windir).join("Temp"));
        }
        let explicit = std::env::var_os("KOMIKA_POSTGRES_JUNCTION_BASE").map(PathBuf::from);
        for home in alias_homes(explicit, app_data, shared) {
          let (parent, remembered) = match home {
            AliasHome::Own(parent) => (parent, false),
            AliasHome::Shared(base) => (
              base.join(format!("komiq-pg-{}", super::random_secret()?)),
              true,
            ),
          };
          // A shared place: the memo is written before the folder is made (see
          // `reserve_alias`). The app's own place: fixed names, reused or replaced.
          let made = if remembered {
            reserve_alias(&memo, &parent, new_dir)
          } else {
            fs::create_dir_all(&parent).is_ok()
          };
          if !made {
            continue;
          }
          // From here Drop removes whatever was made, also when a step fails.
          let mut paths = Self {
            server: tool_server.clone(),
            real_server: real_server.to_path_buf(),
            runtime: tool_runtime.clone(),
            junction: Some(parent.clone()),
            memo: remembered.then(|| memo.clone()),
          };
          let mut ok = true;
          if need_server {
            let link = parent.join("server");
            ok = junction(&link, real_server).await;
            paths.server = link;
          }
          if ok && need_runtime {
            let link = parent.join("runtime");
            ok = junction(&link, real_runtime).await;
            paths.runtime = link;
          }
          if ok {
            return Ok(paths);
          }
        }
        return Err("Windows needs a writable folder with an ASCII path for its database tools. Set KOMIKA_POSTGRES_JUNCTION_BASE to a short ASCII path you can write, then Retry. No data was removed.".into());
      }
    }
    Ok(Self {
      server: tool_server,
      real_server: real_server.to_path_buf(),
      runtime: tool_runtime,
      junction: None,
      memo: None,
    })
  }
}
impl Drop for ToolPaths {
  fn drop(&mut self) {
    if let Some(parent) = self.junction.take() {
      // remove_dir removes a junction itself and never recurses into what it points to.
      let _ = fs::remove_dir(parent.join("server"));
      let _ = fs::remove_dir(parent.join("runtime"));
      let _ = fs::remove_dir(parent);
    }
    if let Some(memo) = self.memo.take() {
      let _ = fs::remove_file(memo);
    }
  }
}

pub fn free_bytes(path: &Path) -> Result<Option<u64>, String> {
  #[cfg(unix)]
  {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: zeroed POD statvfs, valid terminated path; output is read on success.
    unsafe {
      let mut info: libc::statvfs = std::mem::zeroed();
      if libc::statvfs(c.as_ptr(), &mut info) != 0 {
        return Err(format!(
          "Could not check backup free space: {}",
          std::io::Error::last_os_error()
        ));
      }
      #[allow(clippy::unnecessary_cast)]
      return Ok(Some(
        (info.f_bavail as u64).saturating_mul(info.f_frsize as u64),
      ));
    }
  }
  #[cfg(windows)]
  {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut bytes = 0;
    // SAFETY: terminated path and writable u64, optional outputs null.
    if unsafe {
      GetDiskFreeSpaceExW(
        wide.as_ptr(),
        &mut bytes,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
      )
    } == 0
    {
      return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Some(bytes))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  #[test]
  fn junction_only_for_windows_non_ascii() {
    assert!(needs_junction(true, Path::new("C:/Users/José/漢字")));
    assert!(!needs_junction(true, Path::new("C:/Users/with space")));
    assert!(!needs_junction(false, Path::new("/Users/José/漢字")));
  }

  /// Absolute on the system the test runs on (`C:\…` is not absolute on Unix).
  fn abs(path: &str) -> PathBuf {
    if cfg!(windows) {
      PathBuf::from(format!("C:{}", path.replace('/', "\\")))
    } else {
      PathBuf::from(path)
    }
  }

  /// B4: the alias lives in the app's own data folder under fixed names whenever that
  /// folder's path is ASCII; a non-ASCII data folder cannot hold an ASCII alias, so only
  /// then a shared temporary folder is used, and never one whose own path is not ASCII.
  #[test]
  fn alias_homes_prefer_the_apps_own_folder_and_skip_non_ascii_places() {
    let temp = abs("/Windows/Temp");
    let ascii = abs("/Users/joe/AppData/Roaming/app.komika.desktop");
    assert_eq!(
      alias_homes(None, &ascii, vec![temp.clone()]),
      vec![
        AliasHome::Own(ascii.join("pg-alias")),
        AliasHome::Shared(temp.clone())
      ],
      "fixed place in the app's own data folder first"
    );
    let accented = abs("/Users/José 漢字/AppData/Roaming/app.komika.desktop");
    let profile_temp = abs("/Users/José 漢字/AppData/Local/Temp");
    assert_eq!(
      alias_homes(
        None,
        &accented,
        vec![profile_temp, temp.clone(), temp.clone()]
      ),
      vec![AliasHome::Shared(temp.clone())],
      "neither the data folder nor the profile's temporary folder is ASCII"
    );
    // The owner's explicit folder comes first, with one fixed folder per data folder.
    let base = abs("/pg");
    let homes = alias_homes(Some(base.clone()), &accented, vec![temp.clone()]);
    assert_eq!(homes.len(), 2);
    let AliasHome::Own(first) = &homes[0] else {
      panic!("{homes:?}")
    };
    assert_eq!(first.parent(), Some(base.as_path()));
    assert_eq!(
      homes,
      alias_homes(Some(base.clone()), &accented, vec![temp.clone()]),
      "the same name at every start"
    );
    assert_ne!(
      homes[0],
      alias_homes(Some(base), &ascii, vec![temp.clone()])[0],
      "another data folder gets another name"
    );
    // A relative or non-ASCII explicit folder is not used.
    for bad in [PathBuf::from("relative"), abs("/Users/José")] {
      assert_eq!(
        alias_homes(Some(bad), &accented, vec![temp.clone()]),
        vec![AliasHome::Shared(temp.clone())]
      );
    }
  }

  /// B-1: `canonicalize` gives `\\?\` paths on Windows; the tools get the plain form
  /// whenever it names the same file. Pure text, so it runs on every system.
  #[test]
  fn verbatim_windows_paths_are_given_to_tools_in_their_plain_form() {
    for (verbatim, plain) in [
      (r"\\?\C:\x", r"C:\x"),
      (r"\\?\C:\", r"C:\"),
      (
        r"\\?\C:\Users\José 漢字\AppData\Roaming\app.komika.desktop\server",
        r"C:\Users\José 漢字\AppData\Roaming\app.komika.desktop\server",
      ),
      (
        r"\\?\d:\Program Files\Komiq Server\postgres\x86_64-windows\18",
        r"d:\Program Files\Komiq Server\postgres\x86_64-windows\18",
      ),
      (r"\\?\UNC\server\share", r"\\server\share"),
      (
        r"\\?\UNC\server\share\data folder\x",
        r"\\server\share\data folder\x",
      ),
      (r"\\?\C:\data\", r"C:\data\"),
      (r"\\?\C:\.config\a.b.c\nul.d", r"C:\.config\a.b.c\nul.d"),
    ] {
      let got = plain_form(verbatim);
      if plain.ends_with(r"nul.d") {
        // `NUL` with an extension is still the device in the plain form.
        assert_eq!(got, None, "{verbatim}");
        continue;
      }
      assert_eq!(got.as_deref(), Some(plain), "{verbatim}");
    }
    let long = format!(r"\\?\C:\{}", "a".repeat(260));
    for kept in [
      r"C:\x",                                               // already plain
      r"\\server\share\x",                                   // already plain
      r"/Users/joe/data",                                    // not a Windows path
      r"\\.\C:\x",                                           // a device path
      r"\\?\Volume{b75e2c83-0000-0000-0000-602f00000000}\x", // no drive letter
      r"\\?\GLOBALROOT\Device\HarddiskVolume1\x",
      r"\\?\UNC\server",   // no share
      r"\\?\UNC\\share\x", // no server
      r"\\?\C:",           // drive-relative
      r"\\?\C:x",
      r"\\?\C:\data.",   // only the verbatim form keeps the dot
      r"\\?\C:\data \x", // or the closing space
      r"\\?\C:\a\..\b",  // `..` is a name in the verbatim form
      r"\\?\C:\a\.\b",
      r"\\?\C:\a/b", // `/` is a name character there
      r"\\?\C:\a\con",
      r"\\?\C:\a\COM1.txt",
      r"\\?\C:\a\b?",
      r"\\?\C:\a\b:stream",
      long.as_str(), // beyond the old path limit
    ] {
      assert_eq!(plain_form(kept), None, "{kept}");
    }
    // Just below the limit it is still converted.
    let fits = format!(r"\\?\C:\{}", "a".repeat(256));
    assert_eq!(plain_form(&fits).map(|p| p.len()), Some(259));
    // `tool_path` changes nothing on a system that has no verbatim paths.
    let path = PathBuf::from(r"\\?\C:\x");
    assert_eq!(
      tool_path(path.clone()),
      if cfg!(windows) {
        PathBuf::from(r"C:\x")
      } else {
        path
      }
    );
    assert_eq!(tool_path(PathBuf::from("/a b/é")), PathBuf::from("/a b/é"));
  }

  /// B-2: the memo is written before the alias folder is made, so an app killed between
  /// the two steps leaves nothing that the next start cannot remove.
  #[test]
  fn the_alias_memo_is_written_before_the_alias_folder_is_made() {
    let root = std::env::temp_dir().join(format!("komiq-alias-order-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("shared")).unwrap();
    let memo = root.join(ALIAS_MEMO);
    let parent = root.join("shared").join("komiq-pg-0123456789abcdef");

    // At the moment the folder is made, the memo already names it.
    let seen = std::cell::Cell::new(false);
    assert!(reserve_alias(&memo, &parent, |dir| {
      assert_eq!(dir, parent);
      assert_eq!(fs::read_to_string(&memo).unwrap(), parent.to_string_lossy());
      assert!(!parent.exists());
      seen.set(true);
      fs::create_dir(dir)
    }));
    assert!(seen.get() && parent.is_dir() && memo.is_file());
    // The next start removes the folder and the memo.
    forget_remembered_alias(&memo);
    assert!(!parent.exists() && !memo.exists());

    // Killed between the two steps: only the memo is there. The next start removes it
    // and nothing else.
    fs::write(&memo, parent.to_string_lossy().as_bytes()).unwrap();
    fs::write(root.join("shared/bystander"), "kept").unwrap();
    forget_remembered_alias(&memo);
    assert!(!memo.exists());
    assert_eq!(
      fs::read_to_string(root.join("shared/bystander")).unwrap(),
      "kept"
    );

    // The folder cannot be made: the memo is taken back and nothing is claimed.
    assert!(!reserve_alias(&memo, &parent, |_| Err(
      std::io::Error::other("no")
    )));
    assert!(!memo.exists() && !parent.exists());
    // A folder of that name that is already there (not ours) fails the same way and stays.
    fs::create_dir(&parent).unwrap();
    assert!(!reserve_alias(&memo, &parent, |dir| fs::create_dir(dir)));
    assert!(!memo.exists() && parent.is_dir());
    // The memo cannot be written (its folder is missing): the alias folder is not made.
    let nowhere = root.join("missing").join(ALIAS_MEMO);
    assert!(!reserve_alias(
      &nowhere,
      &parent.with_extension("b"),
      |_| { panic!("no folder may be made without a memo") }
    ));
    fs::remove_dir_all(root).unwrap();
  }

  /// What a force-quit app left in a shared place is removed at the next start: the
  /// alias entries and their folder, nothing behind them, and nothing that is not ours.
  #[test]
  fn a_remembered_alias_folder_is_removed_without_touching_what_it_points_to() {
    let root = std::env::temp_dir().join(format!("komiq-alias-test-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let real = root.join("real data é");
    fs::create_dir_all(&real).unwrap();
    fs::write(real.join("precious"), "data").unwrap();
    let parent = root.join("komiq-pg-0123456789abcdef");
    fs::create_dir_all(&parent).unwrap();
    // Stand-ins for the junctions (a junction is removed like an empty folder).
    fs::create_dir(parent.join("server")).unwrap();
    fs::create_dir(parent.join("runtime")).unwrap();
    let memo = root.join(ALIAS_MEMO);
    fs::write(&memo, parent.to_string_lossy().as_bytes()).unwrap();
    forget_remembered_alias(&memo);
    assert!(!parent.exists() && !memo.exists());
    assert_eq!(fs::read_to_string(real.join("precious")).unwrap(), "data");

    // A remembered path that is not an alias folder of ours is never removed.
    let other = root.join("someone elses folder");
    fs::create_dir_all(other.join("server")).unwrap();
    fs::write(&memo, other.to_string_lossy().as_bytes()).unwrap();
    forget_remembered_alias(&memo);
    assert!(other.join("server").exists());
    assert!(!memo.exists());
    // A folder with content is left in place (remove_dir is not recursive).
    fs::create_dir_all(parent.join("server")).unwrap();
    fs::write(parent.join("server/file"), "content").unwrap();
    fs::write(&memo, parent.to_string_lossy().as_bytes()).unwrap();
    forget_remembered_alias(&memo);
    assert!(parent.join("server/file").exists());
    // No memo: nothing happens.
    forget_remembered_alias(&memo);
    fs::remove_dir_all(root).unwrap();
  }
}
