#!/usr/bin/env python3
"""Disposable PostgreSQL 18 desktop spike. Python >=3.12, standard library only.

All outputs are private to --work-dir. --force creates a fresh run subdirectory;
it never reuses, removes, or stops anything from an earlier run. Not a supervisor.
"""
import sys
sys.dont_write_bytecode = True

import argparse
import ctypes
import datetime
import hashlib
import json
import os
from pathlib import Path, PurePosixPath, PureWindowsPath
import platform
import plistlib
import re
import secrets
import shutil
import signal
import socket
import struct
import subprocess
import tarfile
import tempfile
import time
import urllib.request

RELEASE = "18.6.0"
ASSETS = {
    "aarch64-apple-darwin": (13081312, "a257bcdb8aa3301a13d6a5bcec48f8c9517045b7cbae71e50788b2615539e95b"),
    "x86_64-apple-darwin": (13435995, None),
    "x86_64-unknown-linux-gnu": (12184736, None),
    "aarch64-unknown-linux-gnu": (11979851, None),
    "x86_64-pc-windows-msvc": (54068902, None),
}
PROGRAMS = ("postgres", "initdb", "pg_ctl", "pg_dump", "pg_restore", "psql", "pg_isready")
MACH = {b"\xfe\xed\xfa\xce", b"\xce\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xcf\xfa\xed\xfe", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca", b"\xca\xfe\xba\xbf", b"\xbf\xba\xfe\xca"}
USER = "komika_spike"
LC_CTYPE_CANDIDATES = ("C.UTF-8", "C.utf8", "en_US.UTF-8", "en_US.utf8", "en-US",
                       "English_United States.utf8", "English_United States.1252")
WINDOWS_TRIAL_CAP = 11
WINDOWS_MATRIX_SECONDS = 780
WINDOWS_TRIAL_SECONDS = 150


def windows_process_entry_type():
    from ctypes import wintypes as w
    class PROCESSENTRY32W(ctypes.Structure):
        _fields_ = [("dwSize", w.DWORD), ("cntUsage", w.DWORD),
                    ("th32ProcessID", w.DWORD), ("th32DefaultHeapID", ctypes.c_size_t),
                    ("th32ModuleID", w.DWORD), ("cntThreads", w.DWORD),
                    ("th32ParentProcessID", w.DWORD), ("pcPriClassBase", w.LONG),
                    ("dwFlags", w.DWORD), ("szExeFile", w.WCHAR * 260)]
    return PROCESSENTRY32W


WINDOWS_PROCESS_ENTRY = windows_process_entry_type()


def windows_kernel32():
    """All Win32 declarations live here, including pointer-sized HANDLE returns."""
    from ctypes import wintypes as w
    k = ctypes.WinDLL("kernel32", use_last_error=True)
    signatures = {
        "GetACP": ([], w.UINT), "GetOEMCP": ([], w.UINT),
        "GetConsoleCP": ([], w.UINT), "GetConsoleOutputCP": ([], w.UINT),
        "GetShortPathNameW": ([w.LPCWSTR, w.LPWSTR, w.DWORD], w.DWORD),
        "GetFileAttributesW": ([w.LPCWSTR], w.DWORD),
        "RemoveDirectoryW": ([w.LPCWSTR], w.BOOL),
        "OpenProcess": ([w.DWORD, w.BOOL, w.DWORD], w.HANDLE),
        "CreateToolhelp32Snapshot": ([w.DWORD, w.DWORD], w.HANDLE),
        "Process32FirstW": ([w.HANDLE, ctypes.POINTER(WINDOWS_PROCESS_ENTRY)], w.BOOL),
        "Process32NextW": ([w.HANDLE, ctypes.POINTER(WINDOWS_PROCESS_ENTRY)], w.BOOL),
        "TerminateProcess": ([w.HANDLE, w.UINT], w.BOOL),
        "WaitForSingleObject": ([w.HANDLE, w.DWORD], w.DWORD),
        "QueryFullProcessImageNameW": ([w.HANDLE, w.DWORD, w.LPWSTR, ctypes.POINTER(w.DWORD)], w.BOOL),
        "GetProcessTimes": ([w.HANDLE] + [ctypes.POINTER(w.FILETIME)] * 4, w.BOOL),
        "GetExitCodeProcess": ([w.HANDLE, ctypes.POINTER(w.DWORD)], w.BOOL),
        "GetCurrentProcess": ([], w.HANDLE),
        "CreateDirectoryW": ([w.LPCWSTR, w.LPVOID], w.BOOL),
        "CreateFileW": ([w.LPCWSTR, w.DWORD, w.DWORD, w.LPVOID, w.DWORD, w.DWORD, w.HANDLE], w.HANDLE),
        "DeviceIoControl": ([w.HANDLE, w.DWORD, w.LPVOID, w.DWORD, w.LPVOID, w.DWORD, ctypes.POINTER(w.DWORD), w.LPVOID], w.BOOL),
        "CloseHandle": ([w.HANDLE], w.BOOL),
    }
    for name, (args, result) in signatures.items():
        fn = getattr(k, name)
        fn.argtypes, fn.restype = args, result
    security = ctypes.WinDLL("advapi32", use_last_error=True)
    for name, (args, result) in {
        "OpenProcessToken": ([w.HANDLE, w.DWORD, ctypes.POINTER(w.HANDLE)], w.BOOL),
        "GetTokenInformation": ([w.HANDLE, ctypes.c_int, w.LPVOID, w.DWORD, ctypes.POINTER(w.DWORD)], w.BOOL),
        "ImpersonateLoggedOnUser": ([w.HANDLE], w.BOOL),
        "RevertToSelf": ([], w.BOOL),
    }.items():
        fn = getattr(security, name)
        fn.argtypes, fn.restype = args, result
        setattr(k, name, fn)
    return k


def windows_error(operation):
    code = ctypes.get_last_error()
    return OSError(code, operation + ": " + ctypes.FormatError(code))


def windows_code_pages(k=None):
    k = k or windows_kernel32()
    acp = k.GetACP()
    return {"GetACP": acp, "GetOEMCP": k.GetOEMCP(),
            "GetConsoleCP": k.GetConsoleCP(), "GetConsoleOutputCP": k.GetConsoleOutputCP(),
            "process_code_page_is_utf8": acp == 65001,
            "console_zero_means_no_console": True}


def windows_short_path(path, k=None):
    k = k or windows_kernel32()
    size = k.GetShortPathNameW(str(path), None, 0)
    if not size:
        raise windows_error("GetShortPathNameW size")
    buf = ctypes.create_unicode_buffer(size)
    used = k.GetShortPathNameW(str(path), buf, size)
    if not used:
        raise windows_error("GetShortPathNameW")
    if used >= size:
        raise RuntimeError("Short path buffer changed size")
    # With 8.3 disabled Windows can return the original spelling, successfully.
    if not buf.value.isascii():
        return None
    return Path(buf.value)


def windows_create_junction(link, target, run):
    # EncodedCommand avoids cmd.exe parsing/expansion of arbitrary Unicode paths.
    import base64
    def quote(value):
        return "'" + str(value).replace("'", "''") + "'"
    script = ("$ErrorActionPreference='Stop'; New-Item -ItemType Junction -Path "
              + quote(link) + " -Target " + quote(target) + " | Out-Null")
    return run("powershell", "-NoProfile", "-NonInteractive", "-EncodedCommand",
               base64.b64encode(script.encode("utf-16le")).decode("ascii"))


def windows_remove_junction(link, k=None):
    """Remove only the directory link: never recurse through its target."""
    k = k or windows_kernel32()
    attrs = k.GetFileAttributesW(str(link))
    if attrs == 0xffffffff:
        raise windows_error("GetFileAttributesW junction")
    if attrs & 0x410 != 0x410:  # FILE_ATTRIBUTE_REPARSE_POINT | DIRECTORY
        raise RuntimeError("Refusing to remove a path that is not a directory reparse point")
    if not k.RemoveDirectoryW(str(link)):
        raise windows_error("RemoveDirectoryW junction")


def windows_native_junction(link, target, k):
    """Create a mount-point reparse point under the caller's impersonation token."""
    from ctypes import wintypes as w
    absolute = str(target.resolve())
    substitute = "\\??\\" + absolute if not absolute.startswith("\\\\") else "\\??\\UNC\\" + absolute[2:]
    sub, display = substitute.encode("utf-16le"), absolute.encode("utf-16le")
    paths = sub + b"\0\0" + display + b"\0\0"
    raw = struct.pack("<IHHHHHH", 0xa0000003, len(paths) + 8, 0, 0, len(sub), len(sub) + 2, len(display)) + paths
    if not k.CreateDirectoryW(str(link), None):
        raise windows_error("CreateDirectoryW junction")
    h = k.CreateFileW(str(link), 0x40000000, 0, None, 3, 0x02200000, None)
    if h == ctypes.c_void_p(-1).value:
        error = windows_error("CreateFileW junction")
        if not k.RemoveDirectoryW(str(link)):
            raise RuntimeError(str(error) + "; " + str(windows_error("RemoveDirectoryW failed junction")))
        raise error
    try:
        buf = ctypes.create_string_buffer(raw)
        returned = w.DWORD()
        if not k.DeviceIoControl(h, 0x000900a4, buf, len(raw), None, 0, ctypes.byref(returned), None):
            raise windows_error("FSCTL_SET_REPARSE_POINT")
    except Exception as error:
        cleanup_errors = []
        if not k.CloseHandle(h):
            cleanup_errors.append(str(windows_error("CloseHandle failed junction")))
        if not k.RemoveDirectoryW(str(link)):  # Empty directory if setting the reparse point failed.
            cleanup_errors.append(str(windows_error("RemoveDirectoryW failed junction")))
        if cleanup_errors:
            raise RuntimeError(str(error) + "; " + "; ".join(cleanup_errors)) from error
        raise
    else:
        if not k.CloseHandle(h):
            raise windows_error("CloseHandle junction")


def windows_standard_junction_probe(link, target, details, k=None):
    """Use the current standard token or the elevated runner's linked UAC token."""
    from ctypes import wintypes as w
    k = k or windows_kernel32()
    token, linked, used = w.HANDLE(), w.HANDLE(), w.DWORD()
    impersonating = False
    created = False
    try:
        if not k.OpenProcessToken(k.GetCurrentProcess(), 0x000a, ctypes.byref(token)):
            raise windows_error("OpenProcessToken")
        elevation = w.DWORD()
        if not k.GetTokenInformation(token, 20, ctypes.byref(elevation), ctypes.sizeof(elevation), ctypes.byref(used)):
            raise windows_error("TokenElevation")
        details["runner_elevated"] = bool(elevation.value)
        selected = token
        if elevation.value:
            if not k.GetTokenInformation(token, 19, ctypes.byref(linked), ctypes.sizeof(linked), ctypes.byref(used)):
                raise windows_error("TokenLinkedToken; standard-user permission unmeasured")
            selected = linked
            if not k.GetTokenInformation(selected, 20, ctypes.byref(elevation), ctypes.sizeof(elevation), ctypes.byref(used)):
                raise windows_error("Linked TokenElevation")
            if elevation.value:
                raise RuntimeError("Linked token is elevated; standard-user permission unmeasured")
        details["token_source"] = "linked UAC token" if linked.value else "current token"
        if not k.ImpersonateLoggedOnUser(selected):
            raise windows_error("ImpersonateLoggedOnUser")
        impersonating = True
        details["standard_user_token"] = True
        windows_native_junction(link, target, k)
        created = details["created"] = True
        if not link.samefile(target):
            raise RuntimeError("Junction does not resolve to the private target")
        details["verified"] = True
    finally:
        try:
            if created:
                windows_remove_junction(link, k)
                details["removed"] = True
        finally:
            # Revert before propagating any probe/cleanup error.
            if impersonating and not k.RevertToSelf():
                raise windows_error("RevertToSelf")
            for handle in (linked, token):
                if handle.value and not k.CloseHandle(handle):
                    raise windows_error("CloseHandle token")


def windows_identity(pid, k=None):
    from ctypes import wintypes as w
    k = k or windows_kernel32()
    h = k.OpenProcess(0x1000, False, pid)
    if not h:
        raise windows_error("OpenProcess")
    try:
        return windows_handle_identity(h, k)
    finally:
        if not k.CloseHandle(h):
            raise windows_error("CloseHandle")


def windows_handle_identity(h, k):
    from ctypes import wintypes as w
    size = w.DWORD(32768)
    buf = ctypes.create_unicode_buffer(size.value)
    times = [w.FILETIME() for _ in range(4)]
    if not k.QueryFullProcessImageNameW(h, 0, buf, ctypes.byref(size)):
        raise windows_error("QueryFullProcessImageNameW")
    if not k.GetProcessTimes(h, *(ctypes.byref(t) for t in times)):
        raise windows_error("GetProcessTimes")
    return (str(Path(buf.value).resolve()), times[0].dwHighDateTime, times[0].dwLowDateTime)


def windows_processes(k=None):
    """Wide Toolhelp enumeration; no command-line decoding or NtQueryInformationProcess."""
    k = k or windows_kernel32()
    h = k.CreateToolhelp32Snapshot(2, 0)  # TH32CS_SNAPPROCESS
    if h == ctypes.c_void_p(-1).value:
        raise windows_error("CreateToolhelp32Snapshot")
    rows = []
    entry = WINDOWS_PROCESS_ENTRY()
    entry.dwSize = ctypes.sizeof(entry)
    try:
        ok = k.Process32FirstW(h, ctypes.byref(entry))
        while ok:
            rows.append({"pid": int(entry.th32ProcessID), "parent": int(entry.th32ParentProcessID),
                         "name": entry.szExeFile})
            ok = k.Process32NextW(h, ctypes.byref(entry))
        if ctypes.get_last_error() != 18:  # ERROR_NO_MORE_FILES; all other errors invalidate absence.
            raise windows_error("Process32 enumeration")
        return rows
    finally:
        if not k.CloseHandle(h):
            raise windows_error("CloseHandle snapshot")


def windows_private_processes(runtime):
    owned = {}
    for row in windows_processes():
        if row["name"].lower() != "postgres.exe":
            continue
        try:
            ident = windows_identity(row["pid"])
            if contained(Path(ident[0]), runtime) and windows_process_alive(row["pid"]):
                owned[row["pid"]] = ident
        except OSError as e:
            if e.errno != 87:  # A process exited between snapshot and query.
                raise
    return owned


def windows_terminate_owned(pid, ident, runtime, k=None):
    """Validate image AND creation time on the same handle used for termination."""
    if not contained(Path(ident[0]), runtime):
        raise RuntimeError("Refusing termination outside this run's private runtime")
    k = k or windows_kernel32()
    h = k.OpenProcess(0x101001, False, pid)  # SYNCHRONIZE | QUERY_LIMITED_INFORMATION | TERMINATE
    if not h:
        if ctypes.get_last_error() == 87:
            return False
        raise windows_error("OpenProcess termination")
    try:
        if windows_handle_identity(h, k) != ident:
            return False  # Reused PID.
        if not k.TerminateProcess(h, 1):
            raise windows_error("TerminateProcess")
        if k.WaitForSingleObject(h, 5000) != 0:
            raise RuntimeError("Identified private process did not exit within 5 seconds")
        return True
    finally:
        if not k.CloseHandle(h):
            raise windows_error("CloseHandle termination")


def windows_process_alive(pid, k=None):
    from ctypes import wintypes as w
    k = k or windows_kernel32()
    h = k.OpenProcess(0x1000, False, pid)
    if not h:
        if ctypes.get_last_error() == 87:  # ERROR_INVALID_PARAMETER: PID no longer exists.
            return False
        raise windows_error("OpenProcess cleanup")
    try:
        code = w.DWORD()
        if not k.GetExitCodeProcess(h, ctypes.byref(code)):
            raise windows_error("GetExitCodeProcess")
        return code.value == 259  # STILL_ACTIVE
    finally:
        if not k.CloseHandle(h):
            raise windows_error("CloseHandle")


def decode_output(raw, code_pages=None):
    """Never let diagnostic bytes prevent lifecycle work; return text and decoder."""
    encodings = ["utf-8"]
    if code_pages is None and os.name == "nt":
        try:
            k = windows_kernel32()
            code_pages = (k.GetACP(), k.GetOEMCP())
        except Exception:
            code_pages = ()
    encodings.extend("cp" + str(cp) for cp in (code_pages or ()) if cp)
    for encoding in dict.fromkeys(encodings):
        try:
            return raw.decode(encoding).replace("\r\n", "\n"), encoding
        except (UnicodeError, LookupError):
            pass
    return raw.decode("utf-8", errors="replace").replace("\r\n", "\n"), "utf-8/replace"


def runtime_exclusion(name):
    base = PurePosixPath(name).name.lower()
    if (re.match(r"(?:lib)?(?:ecpg|pgtypes)", base)
            or base in ("sqlca.h", "sqlda.h")):
        return "embedded-SQL client library/tool/header; not used by server or product tools"
    if re.match(r"(?:lib)?uuid-ossp(?:\.|--)", base):
        return "uuid-ossp is not used; gen_random_uuid() is built in"
    return None


def interpreter_extension(name):
    # Includes *u controls and all *_plpython3 / *_plperl / *_pltcl transforms.
    return bool(re.fullmatch(r"(?:plpython[0-9]*(?:u)?|plperl(?:u)?|pltcl(?:u)?|.+_plpython[0-9]*|.+_plperl|.+_pltcl)", name, re.I))


def control_module(text):
    match = re.search(r"^\s*module_pathname\s*=\s*'([^']+)'", text, re.M)
    return match[1] if match else None


def module_candidates(root, module, names):
    if not module.startswith("$libdir/"):
        return []  # No external absolute module may become part of this bundle.
    stem = module[len("$libdir/"):]
    return [n for n in names if PurePosixPath(n).parts[0].startswith("lib")
            and PurePosixPath(n).name in {stem + ext for ext in (".so", ".dylib", ".dll")}
            | {"lib" + stem + ext for ext in (".so", ".dylib", ".dll")}]



def host_triple():
    arch = {"arm64": "aarch64", "aarch64": "aarch64", "amd64": "x86_64", "x86_64": "x86_64"}.get(platform.machine().lower())
    suffix = {"Darwin": "apple-darwin", "Linux": "unknown-linux-gnu", "Windows": "pc-windows-msvc"}.get(platform.system())
    triple = f"{arch}-{suffix}"
    if triple not in ASSETS:
        raise ValueError(f"Unsupported host: {platform.system()} / {platform.machine()}")
    return triple


def contained(path, root):
    try:
        path.resolve().relative_to(root.resolve())
        return True
    except ValueError:
        return False


def safe_path(name):
    # Backslashes and drives must be checked even when inspecting Windows on Unix.
    p = PurePosixPath(name.replace("\\", "/"))
    w = PureWindowsPath(name)
    if p.is_absolute() or w.drive or w.root or ".." in p.parts or ":" in name:
        raise ValueError(f"Unsafe archive path: {name!r}")
    return p


def extract_safe(archive, dest):
    if not hasattr(tarfile, "data_filter"):
        raise RuntimeError("Python >=3.12 with tarfile.data_filter is required")
    dest.mkdir()
    with tarfile.open(archive, "r:gz") as tf:
        seen = set()
        members = tf.getmembers()
        for m in members:
            p = safe_path(m.name)
            key = str(p).casefold() if os.name == "nt" else str(p)
            if key in seen:
                raise ValueError(f"Duplicate archive path: {m.name!r}")
            seen.add(key)
            if not (m.isfile() or m.isdir() or m.issym() or m.islnk()):
                raise ValueError(f"Unsupported archive member: {m.name!r}")
            if m.issym() or m.islnk():
                link = safe_path(m.linkname)
                target = dest.joinpath(*(p.parent / link if m.issym() else link).parts)
                if not contained(target, dest):
                    raise ValueError(f"Escaping archive link: {m.name!r}")
        tf.extractall(dest, members=members, filter="data")
    for p in dest.rglob("*"):
        if not contained(p, dest):
            raise ValueError(f"Extracted link escapes bundle: {p}")


def files(root):
    return sorted((p for p in root.rglob("*") if p.is_file() or p.is_symlink()), key=lambda p: p.as_posix())


def tree_size(root):
    # Logical file bytes; symlinks counted as links, not their target again.
    return sum(p.lstat().st_size for p in files(root))


def magic(path):
    with path.open("rb") as f:
        return f.read(4)


def find_bundle(extracted, triple):
    exe = "postgres.exe" if "windows" in triple else "postgres"
    roots = [p.parent.parent for p in extracted.rglob(exe) if p.parent.name == "bin"]
    if len(roots) != 1:
        raise RuntimeError(f"Expected exactly one bin/{exe}; found {len(roots)}")
    return roots[0]


def inventory(root, triple):
    suffix = ".exe" if "windows" in triple else ""
    missing = [n for n in PROGRAMS if not (root / "bin" / (n + suffix)).is_file()]
    if missing:
        raise RuntimeError(f"Missing bundled programs: {', '.join(missing)}")
    all_files = files(root)
    keep = []
    excluded = []
    for p in all_files:
        r = p.relative_to(root)
        reason = runtime_exclusion(r.as_posix())
        if reason:
            excluded.append({"file": r.as_posix(), "reason": reason})
            continue
        # pgxs contains build/test tools, never needed by a running server.
        if "pgxs" in r.parts:
            continue
        # Conservative runtime closure: retain other lib/share files, extensions,
        # DLLs beside executables, runtime notices, and links. No dependency guessing.
        if (r.parts[0].lower().startswith("lib") or r.parts[0] == "share"
                or re.search(r"\.(?:dll|dylib|so)(?:\.[0-9.]+)?$", p.name, re.I)
                or (r.parts[0] == "bin" and (p.name in [n + suffix for n in PROGRAMS] or p.suffix.lower() == ".dll"))
                or p.is_symlink() or re.search(r"license|licence|copyright|notice|copying", p.name, re.I)):
            keep.append(r.as_posix())
    # Links in the runtime need their targets too (including chains).
    chosen = set(keep)
    for name in list(chosen):
        p = root / name
        if p.is_symlink():
            if not contained(p, root):
                raise RuntimeError(f"Runtime link escapes root: {name}")
            target = p.resolve()
            if target.is_dir():
                chosen.update(q.relative_to(root.resolve()).as_posix() for q in files(target))
            else:
                chosen.add(target.relative_to(root.resolve()).as_posix())
    for name in sorted(chosen):
        base = PurePosixPath(name).name
        extension = base.split("--", 1)[0].split(".", 1)[0]
        if extension.startswith("lib") and re.search(r"\.(so|dll|dylib)", base):
            extension = extension[3:]
        reason = runtime_exclusion(name)
        if reason or interpreter_extension(extension):
            chosen.remove(name)
            if not any(r["file"] == name for r in excluded):
                excluded.append({"file": name, "reason": reason or "procedural language/transform requires an interpreter the app does not ship"})
    controls = [p for p in all_files if p.suffix == ".control"]
    control_checks = []
    for control in controls:
        name = control.relative_to(root).as_posix()
        if name not in chosen:
            continue
        module = control_module(control.read_text(encoding="utf-8", errors="replace"))
        if module is None:
            control_checks.append({"control": name, "module_pathname": None, "status": "no-module-declared"})
            continue
        modules_found = module_candidates(root, module, chosen)
        rec = {"control": name, "module_pathname": module, "modules": modules_found,
               "status": "present" if modules_found else "excluded-missing-module"}
        control_checks.append(rec)
        if not modules_found:
            for drop in sorted(chosen.copy()):
                b = PurePosixPath(drop).name
                if drop == name or (b.startswith(control.stem + "--") and b.endswith(".sql")):
                    chosen.remove(drop)
                    excluded.append({"file": drop, "reason": "control module_pathname library absent from runtime", "module_pathname": module})
    sql = [p for p in all_files if p.name.startswith("pg_trgm--") and p.suffix == ".sql"]
    modules = [p for p in all_files if re.fullmatch(r"(?:lib)?pg_trgm\.(?:so|dylib|dll)", p.name)]
    result = {
        "full_bytes": tree_size(root), "size_kind": "logical bytes, symlinks counted once as links",
        "runtime_bytes": sum((root / r).lstat().st_size for r in chosen),
        "runtime_selection": "conservative candidate: excludes pgxs, embedded SQL, uuid-ossp, interpreter languages/transforms, and controls/SQL with absent modules; retains other lib*/share and adjacent DLLs",
        "excluded_findings": excluded, "control_module_checks": control_checks,
        "runtime_files": sorted(chosen),
        "drop_files": [p.relative_to(root).as_posix() for p in all_files if p.relative_to(root).as_posix() not in chosen],
        "extensions": [{"name": p.stem, "control": p.relative_to(root).as_posix(),
                        "sql": [q.relative_to(root).as_posix() for q in all_files if q.name.startswith(p.stem + "--") and q.suffix == ".sql"],
                        "control_text": p.read_text(encoding="utf-8", errors="replace")} for p in controls],
        "pg_trgm": {"control": [p.relative_to(root).as_posix() for p in controls if p.stem == "pg_trgm"],
                    "sql": [p.relative_to(root).as_posix() for p in sql],
                    "module": [p.relative_to(root).as_posix() for p in modules]},
    }
    return result


def copy_runtime(root, names, dest):
    dest.mkdir()
    for name in names:
        src, target = root / name, dest / name
        if not contained(src, root) or not contained(target, dest):
            raise ValueError(f"Unsafe copy path: {name}")
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(src, target, follow_symlinks=False)
    for p in files(dest):
        if not contained(p, dest) or (p.is_symlink() and not p.exists()):
            raise RuntimeError(f"Broken or escaping runtime link: {p}")


class Spike:
    def __init__(self, work, triple, json_out, output_root=None, summary_md=None, sign_identity=None):
        self.work, self.triple, self.json_out = work, triple, json_out
        self.output_root = output_root or work
        self.summary_md, self.sign_identity = summary_md, sign_identity
        self.password = secrets.token_urlsafe(32)
        self.bundle = None
        self.runtime = None
        self.runtime_files = None
        self.clusters = []
        self.command_cwd = work
        self.command_output_dir = work
        self.command_deadline = None
        self.started_monotonic = time.monotonic()
        self.result = {"schema_version": 3, "release": RELEASE, "triple": triple,
                       "host": {"system": platform.system(), "release": platform.release(), "machine": platform.machine(), "python": sys.version},
                       "started_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
                       "work_dir": str(work), "steps": [], "commands": [],
                       "windows_path_matrix": {"applicable": "windows" in triple}}
        self.env = {k: v for k, v in os.environ.items() if not k.upper().startswith(("PG", "LD_", "DYLD_"))}
        self.env.update({"TMPDIR": str(work), "TMP": str(work), "TEMP": str(work), "LC_ALL": "C", "LANG": "C",
                         "PGPASSFILE": str(work / "empty.pgpass"), "PGSSLMODE": "disable", "PGGSSENCMODE": "disable", "PGCONNECT_TIMEOUT": "5", "PGCLIENTENCODING": "UTF8"})
        pgpass = work / "empty.pgpass"
        if os.name == "nt":
            pgpass.touch(exist_ok=False)
        else:
            fd = os.open(pgpass, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            os.close(fd)

    def redact(self, value):
        if isinstance(value, str):
            return value.replace(self.password, "[REDACTED]")
        if isinstance(value, list):
            return [self.redact(v) for v in value]
        if isinstance(value, dict):
            return {k: self.redact(v) for k, v in value.items()}
        return value

    def save(self):
        if not contained(self.json_out, self.output_root) or self.json_out.is_symlink():
            raise ValueError("Unsafe JSON output path")
        self.json_out.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.json_out.with_name(self.json_out.name + "." + secrets.token_hex(8) + ".tmp")
        if not contained(tmp, self.output_root) or tmp.is_symlink():
            raise ValueError("Unsafe JSON temporary path")
        try:
            with tmp.open("x", encoding="utf-8") as f:
                f.write(json.dumps(self.redact(self.result), ensure_ascii=False, indent=2) + "\n")
            tmp.replace(self.json_out)
        finally:
            tmp.unlink(missing_ok=True)

    def persist(self):
        for label, writer in [("JSON", self.save), ("Markdown", self.save_summary)]:
            try:
                writer()
            except (OSError, ValueError) as e:
                message = self.redact(str(e))
                self.result.setdefault("output_errors", []).append(message)
                print(label + " write failed: " + message, file=sys.stderr, flush=True)

    def save_summary(self):
        if self.summary_md is None:
            return
        if not contained(self.summary_md, self.output_root) or self.summary_md.is_symlink():
            raise ValueError("Unsafe Markdown output path")
        def cell(value):
            return str(self.redact(value)).replace("|", "\\|").replace("\n", " ")
        rows = [f"PostgreSQL {RELEASE} — {self.triple}", "", "| Step / sub-result | Status | Seconds | Key numbers / finding |",
                "| --- | --- | ---: | --- |"]
        def visit(label, node):
            data = node.get("details", node)
            metrics = [f"{k}={v}" for k, v in data.items()
                       if isinstance(v, (int, float, bool)) or (isinstance(v, str) and
                       (k.endswith(("_result", "_rows", "_sha256")) or k in ("sha256", "outcome", "team_id", "postgres_version")))]
            for key in ("runtime_failures", "excluded_findings"):
                if key in data:
                    metrics.append(f"{key}={len(data[key])}")
            metrics.append(node.get("error", node.get("reason", "")))
            if "status" in node:
                rows.append(f"| {cell(label)} | {cell(node['status'])} | {node.get('seconds', 0):.3f} | {cell('; '.join(m for m in metrics if m))} |")
            for key, value in data.items():
                if isinstance(value, dict):
                    visit(label + " / " + key, value)
                elif isinstance(value, list):
                    for i, item in enumerate(value):
                        if isinstance(item, dict) and ("status" in item or key == "downloads"):
                            if key == "downloads":
                                item = dict(item, status="fail" if "error" in item else "pass")
                            visit(label + " / " + str(item.get("name", item.get("file", i + 1))), item)
        for step in self.result["steps"]:
            visit(f"{step['step']}. {step['name']}", step)
        rows.extend(self.diagnostic_lines())
        self.summary_md.parent.mkdir(parents=True, exist_ok=True)
        self.summary_md.write_text("\n".join(rows) + "\n", encoding="utf-8")

    def subcheck(self, details, name, fn):
        rec = {"status": "fail", "details": {}}
        details[name] = rec
        before = time.perf_counter()
        try:
            fn(rec["details"])
            rec["status"] = "pass"
        except Exception as e:
            rec["error"] = self.redact(f"{type(e).__name__}: {e}")
        finally:
            rec["seconds"] = time.perf_counter() - before
        return rec

    @staticmethod
    def require_checks(*checks):
        errors = [r["error"] for r in checks if r["status"] == "fail"]
        if errors:
            raise RuntimeError("\n".join(errors))

    def bounded_deadline(self, seconds):
        deadline = time.monotonic() + seconds
        return min(deadline, self.command_deadline) if self.command_deadline is not None else deadline

    def decode(self, raw, record, key):
        value, decoder = decode_output(raw)
        if decoder != "utf-8":
            record[key + "_decode_encoding"] = decoder
        return value

    def run(self, args, check=True, password=False, timeout=180, input_bytes=None, probe_cluster=None):
        if self.command_deadline is not None:
            remaining = self.command_deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("Windows trial command budget exhausted")
            timeout = min(timeout, remaining)
        argv = [str(a) for a in args]
        env = self.env.copy()
        if password:
            env["PGPASSWORD"] = self.password
        before = time.perf_counter()
        rec = {"argv": argv, "cwd": str(self.command_cwd), "timeout": timeout}
        self.result["commands"].append(rec)
        # Files instead of PIPE: a detached PostgreSQL child must not hold open
        # pg_ctl's stdout pipe and deadlock communicate(). Also preserves timeout output.
        out = self.command_output_dir / f"command-{len(self.result['commands']):04d}.stdout"
        err = out.with_suffix(".stderr")
        child = None
        try:
            with out.open("wb") as stdout, err.open("wb") as stderr, tempfile.TemporaryFile() as stdin:
                if input_bytes is not None:
                    stdin.write(input_bytes)
                    stdin.seek(0)
                    rec["stdin_utf8"] = input_bytes.decode("utf-8")
                child = subprocess.Popen(argv, cwd=self.command_cwd, env=env, stdin=stdin if input_bytes is not None else subprocess.DEVNULL,
                                         stdout=stdout, stderr=stderr)
                probe_identity = None
                if probe_cluster is not None and os.name == "nt":
                    try:
                        probe_identity = windows_identity(child.pid)
                    except OSError as e:
                        if e.errno != 87 or child.poll() is None:
                            raise
                    rec["probe_pid"], rec["probe_identity"] = child.pid, probe_identity
                try:
                    rec["returncode"] = child.wait(timeout=timeout)
                except BaseException:
                    if probe_cluster is not None and os.name == "nt":
                        try:
                            self.stop_probe_tree(child.pid, probe_identity, probe_cluster, rec)
                            child.wait(timeout=1)
                        except Exception as e:
                            rec["probe_cleanup_error"] = str(e)
                    else:
                        child.kill()
                        child.wait()
                    raise
        except Exception as e:
            rec["error"] = str(e)
            raise
        finally:
            rec["seconds"] = time.perf_counter() - before
            for key, path in (("stdout", out), ("stderr", err)):
                raw = path.read_bytes() if path.exists() else b""
                rec[key] = self.decode(raw, rec, key)
                if key + "_decode_encoding" in rec:
                    # Hex keeps otherwise ambiguous diagnostics byte-exact, but
                    # never retain a transcript containing the password bytes.
                    rec[key + "_raw_hex"] = raw.replace(self.password.encode(), b"[REDACTED]").hex()
            # Erase secrets even from the private raw transcript files.
            for key, p in (("stdout", out), ("stderr", err)):
                if p.exists():
                    p.write_text(self.redact(rec[key]), encoding="utf-8")
        if check and rec["returncode"] != 0:
            raise RuntimeError(f"Command exited {rec['returncode']}: {argv!r}\n{rec['stdout']}{rec['stderr']}")
        return rec

    def stop_probe_tree(self, pid, ident, c, details):
        # The live recovered server is never a member of the probe's tree.
        details["probe_cleanup"] = []
        try:
            unchanged = ident is not None and windows_identity(pid) == ident
        except OSError as e:
            if e.errno != 87:
                raise
            unchanged = False
        if not unchanged:
            details["probe_pid_changed"] = True
            return
        rows = windows_processes()
        tree = {pid}
        while True:
            expanded = tree | {r["pid"] for r in rows if r["parent"] in tree}
            if expanded == tree:
                break
            tree = expanded
        candidates = []
        for target in tree:
            if target == c["pid"]:
                continue
            try:
                current = ident if target == pid else windows_identity(target)
            except OSError as e:
                if e.errno == 87:
                    continue
                raise
            if current[1:] < ident[1:] or not contained(Path(current[0]), c["root"]):
                details["probe_cleanup"].append({"pid": target, "skipped": "image outside private runtime or older process"})
                continue
            candidates.append((target, current))
        # Stop children first, then pg_ctl; never taskkill /T or kill by image name.
        for target, current in sorted(candidates, key=lambda item: item[0] == pid):
            killed = windows_terminate_owned(target, current, c["root"])
            details["probe_cleanup"].append({"pid": target, "identity": current, "terminated": killed})

    def tool(self, name, *args, **kwargs):
        path = shutil.which(name)
        if not path:
            raise RuntimeError(f"Required inspection tool unavailable: {name}")
        return self.run([path, *args], **kwargs)

    def step(self, number, name, fn):
        rec = {"step": number, "name": name, "status": "fail", "details": {}}
        self.result["steps"].append(rec)
        before = time.perf_counter()
        print(f"[{number}] {name}", flush=True)
        try:
            fn(rec["details"])
            rec["status"] = "pass"
        except KeyboardInterrupt:
            rec["error"] = "Interrupted by user"
            raise
        except Exception as e:
            rec["error"] = self.redact(f"{type(e).__name__}: {e}")
        finally:
            rec["seconds"] = time.perf_counter() - before
            self.persist()
        return rec

    def download(self, details):
        asset = f"postgresql-{RELEASE}-{self.triple}.tar.gz"
        base = f"https://github.com/theseus-rs/postgresql-binaries/releases/download/{RELEASE}/"
        paths = []
        errors = []
        details["downloads"] = []
        for name in (asset, asset + ".sha256"):
            dest = self.work / name
            rec = {"url": base + name, "file": name}
            details["downloads"].append(rec)
            before = time.perf_counter()
            try:
                with urllib.request.urlopen(urllib.request.Request(base + name, headers={"User-Agent": "komika-desktop-spike/1"}), timeout=45) as response, dest.open("wb") as f:
                    limit = ASSETS[self.triple][0] if name == asset else 65536
                    total = 0
                    while True:
                        if time.perf_counter() - before > 180:
                            raise TimeoutError("Download exceeded 180 seconds")
                        block = response.read(1024 * 1024)
                        if not block:
                            break
                        total += len(block)
                        if total > limit:
                            raise ValueError(f"Download exceeds expected maximum of {limit} bytes")
                        f.write(block)
                rec["bytes"] = dest.stat().st_size
                paths.append(dest)
            except Exception as e:
                rec["error"] = str(e)
                errors.append(str(e))
            finally:
                rec["seconds"] = time.perf_counter() - before
        if errors:
            raise RuntimeError("Download failed: " + "; ".join(errors))
        expected_size, pinned = ASSETS[self.triple]
        if paths[0].stat().st_size != expected_size:
            raise RuntimeError(f"Archive size differs: expected {expected_size}, got {paths[0].stat().st_size}")
        checksum_text = paths[1].read_text(encoding="ascii")
        hashes = re.findall(r"(?<![a-fA-F0-9])[a-fA-F0-9]{64}(?![a-fA-F0-9])", checksum_text)
        if len(hashes) != 1:
            raise RuntimeError("Checksum file must contain exactly one SHA-256 digest")
        with paths[0].open("rb") as f:
            actual = hashlib.file_digest(f, "sha256").hexdigest()
        details.update({"sha256": actual, "checksum_file_text": checksum_text, "pinned_sha256": pinned})
        if actual != hashes[0].lower() or (pinned and actual != pinned):
            raise RuntimeError("SHA-256 mismatch (release checksum or independently pinned digest)")
        before = time.perf_counter()
        extract_safe(paths[0], self.work / "unpacked")
        details["extraction_seconds"] = time.perf_counter() - before
        self.bundle = find_bundle(self.work / "unpacked", self.triple)
        details["bundle_root"] = str(self.bundle)

    def require_bundle(self):
        if self.bundle is None:
            raise RuntimeError("Prerequisite unavailable: verified extracted bundle")

    def inspect_inventory(self, details):
        self.require_bundle()
        details.update(inventory(self.bundle, self.triple))
        unpacked = self.work / "unpacked"
        if unpacked.is_dir():
            details["archive_unpacked_bytes"] = tree_size(unpacked)
            details["files_outside_bundle_root"] = [p.relative_to(unpacked).as_posix() for p in files(unpacked) if not contained(p, self.bundle)]
        self.runtime_files = details["runtime_files"]
        self.runtime_exclusions = details["excluded_findings"]
        self.control_checks = details["control_module_checks"]
        copy_runtime(self.bundle, self.runtime_files, self.work / "runtime")
        self.runtime = self.work / "runtime"
        details["relocated_runtime"] = str(self.runtime)
        if not all(details["pg_trgm"].values()):
            raise RuntimeError("pg_trgm must have a control file, SQL scripts, and loadable module")

    def linkage(self, details):
        self.require_bundle()
        details["files"] = []
        details["runtime_failures"] = []
        selection = inventory(self.bundle, self.triple)
        details["excluded_findings"] = [dict(r) for r in selection["excluded_findings"]]
        details["control_module_checks"] = selection["control_module_checks"]
        runtime_names = set(self.runtime_files if self.runtime_files is not None else inventory(self.bundle, self.triple)["runtime_files"])
        failures = details["runtime_failures"]
        system = platform.system()
        if self.triple != host_triple():
            raise RuntimeError("Linkage inspection requires the matching native host")
        if system == "Darwin":
            details["bundle_quarantine"] = self.tool("xattr", "-p", "com.apple.quarantine", self.bundle, check=False)
            details["archive_quarantine"] = self.tool("xattr", "-p", "com.apple.quarantine", self.work / f"postgresql-{RELEASE}-{self.triple}.tar.gz", check=False)
        if system == "Windows":
            dlls = [p.relative_to(self.bundle).as_posix() for p in files(self.bundle) if p.suffix.lower() == ".dll"]
            details["dll_files"] = dlls
            details["adjacent_to_executables"] = [n for n in dlls if PurePosixPath(n).parent == PurePosixPath("bin")]
            details["vc_runtime_files"] = [n for n in dlls if re.match(r"(?:vcruntime|msvcp|msvcr|concrt).*\.dll$", PurePosixPath(n).name, re.I)]
            details["vc_runtime_included"] = bool(details["vc_runtime_files"])
            details["limitation"] = "File presence only; this does not prove a complete PE import or delay-import closure. Runtime steps test loading on this host."
            return
        for p in files(self.bundle):
            if p.is_symlink():
                continue
            m = magic(p)
            if not (m in MACH if system == "Darwin" else m == b"\x7fELF"):
                continue
            item = {"file": p.relative_to(self.bundle).as_posix()}
            item["in_runtime_set"] = item["file"] in runtime_names
            issues = []
            details["files"].append(item)
            try:
                if system == "Darwin":
                    deps = self.tool("otool", "-L", p)
                    item["otool_L"] = deps
                    item["lipo"] = self.tool("lipo", "-archs", p)
                    expected_arch = "arm64" if self.triple.startswith("aarch64") else "x86_64"
                    item["expected_arch_present"] = expected_arch in item["lipo"]["stdout"].split()
                    if not item["expected_arch_present"]:
                        issues.append(f"Expected {expected_arch} slice missing")
                    load = self.tool("otool", "-l", p)
                    rpaths = re.findall(r"cmd LC_RPATH\s+cmdsize \d+\s+path (.*?) \(offset", load["stdout"])
                    item["rpaths"] = rpaths
                    item["outside_paths"] = []
                    item["unresolved_paths"] = []
                    for line in deps["stdout"].splitlines()[1:]:
                        dep = line.strip().split(" (", 1)[0]
                        if not dep:
                            continue
                        if dep.startswith(("/usr/lib/", "/System/")):
                            continue
                        if dep.startswith("/"):
                            if not contained(Path(dep), self.bundle):
                                item["outside_paths"].append(dep)
                        elif dep.startswith("@"):
                            # The main executable affects @executable_path. Test all
                            # retained launcher contexts, without claiming dyld parity.
                            candidates = []
                            for exe in PROGRAMS:
                                base = self.bundle / "bin"
                                def expand(s):
                                    return s.replace("@loader_path", str(p.parent)).replace("@executable_path", str(base))
                                if dep.startswith("@rpath/"):
                                    candidates.extend(Path(expand(r)) / dep[len("@rpath/"):] for r in rpaths)
                                else:
                                    candidates.append(Path(expand(dep)))
                            if not any(q.exists() and contained(q, self.bundle) for q in candidates):
                                item["unresolved_paths"].append(dep)
                        else:
                            item["unresolved_paths"].append(dep)
                    sig = self.tool("codesign", "-dv", "--verbose=4", p, check=False)
                    item["codesign"] = sig
                    raw = sig["stdout"] + sig["stderr"]
                    item["signature"] = {"signed": True if sig["returncode"] == 0 else (False if "not signed at all" in raw else None),
                                         "ad_hoc": "Signature=adhoc" in raw if sig["returncode"] == 0 else None,
                                         "hardened_runtime": "runtime" in raw if sig["returncode"] == 0 else None,
                                         "team_id": re.findall(r"^TeamIdentifier=(.*)$", raw, re.M)}
                    item["quarantine"] = self.tool("xattr", "-p", "com.apple.quarantine", p, check=False)
                    if item["outside_paths"]:
                        issues.append(f"External Mach-O dependencies: {item['outside_paths']}")
                    if item["unresolved_paths"]:
                        issues.append(f"Unresolved Mach-O dependencies: {item['unresolved_paths']}")
                else:
                    item["ldd"] = self.tool("ldd", p, check=False)
                    item["readelf_dynamic"] = self.tool("readelf", "-d", p)
                    versions = self.tool("readelf", "--version-info", p)
                    item["readelf_versions"] = versions
                    needed = re.findall(r"Name: (GLIBC_[0-9.]+)", versions["stdout"])
                    item["glibc_required_versions"] = sorted(set(needed))
                    item["highest_glibc_required"] = max(needed, key=lambda s: tuple(map(int, s[6:].split(".")))) if needed else None
                    item["rpath_runpath_lines"] = [s for s in item["readelf_dynamic"]["stdout"].splitlines() if "RPATH" in s or "RUNPATH" in s]
                    item["missing_libraries"] = [s for s in item["ldd"]["stdout"].splitlines() if "not found" in s]
                    if item["missing_libraries"]:
                        issues.append(f"Missing libraries: {item['missing_libraries']}")
            except Exception as e:
                item["error"] = str(e)
                issues.append(str(e))
            if issues:
                finding = {"file": item["file"], "issues": issues}
                if item["in_runtime_set"]:
                    failures.append(finding)
                else:
                    finding["finding"] = "file must be dropped from the bundle"
                    previous = next((r for r in details["excluded_findings"] if r["file"] == item["file"]), None)
                    if previous is not None:
                        previous.update(finding)
                        if item.get("missing_libraries"):
                            previous["missing_libraries"] = item["missing_libraries"]
                    else:
                        details["excluded_findings"].append(finding)
        if not details["files"]:
            failures.append({"file": None, "issues": ["No native Mach-O/ELF files found"]})
        if system == "Linux":
            versions = [x["highest_glibc_required"] for x in details["files"] if x.get("highest_glibc_required")]
            details["highest_glibc_required"] = max(versions, key=lambda s: tuple(map(int, s[6:].split(".")))) if versions else None
        if failures:
            raise RuntimeError(json.dumps(failures, ensure_ascii=False))

    def new_cluster(self, root, name):
        if root is None:
            raise RuntimeError("Prerequisite unavailable: relocated runtime")
        if self.triple != host_triple():
            raise RuntimeError("Cannot execute a foreign target; run on the matching host")
        c = {"root": root, "data": self.work / (name + " data dir é漢字") / "pg18",
             "socket": self.work / (name + " socket dir"), "log": self.work / (name + "-postgres.log"),
             "initialized": False, "identity": None, "pid": None, "port": None,
             "fixture_seeded": False,
             "start_attempted": False, "descendants": {}}
        self.clusters.append(c)
        return c

    def bin(self, c, name):
        p = c["root"] / "bin" / (name + (".exe" if os.name == "nt" else ""))
        if not p.is_file() or not contained(p, self.work):
            raise RuntimeError(f"Private bundled binary unavailable: {p}")
        return p

    def init(self, c, details):
        # Make absolute references to the original extracted tree fail instead
        # of letting the relocated candidate secretly load its libraries.
        unpacked = self.work / "unpacked"
        if self.bundle is not None and unpacked.is_dir() and contained(self.bundle, unpacked):
            relative = self.bundle.relative_to(unpacked)
            hidden = self.work / "reference-tree-moved"
            unpacked.rename(hidden)
            details["original_tree_moved"] = {"from": str(unpacked), "to": str(hidden)}
            self.bundle = hidden / relative
        version = self.run([self.bin(c, "postgres"), "--version"])
        details["postgres_version"] = version["stdout"].strip()
        if not re.search(r"\bPostgreSQL\)?\s+18\.6(?:\s|$)", version["stdout"]):
            raise RuntimeError(f"Expected upstream PostgreSQL 18.6, got {version['stdout']!r}")
        c["data"].parent.mkdir(parents=True, exist_ok=c.get("windows_trial", False))
        pw = c.get("password_file", self.work / (c["data"].parent.name + "-password.tmp"))
        try:
            # O_EXCL and restrictive POSIX mode; Windows private-work-dir ACL is
            # the boundary (no claim that chmod establishes a Windows ACL).
            fd = os.open(pw, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
            with os.fdopen(fd, "w", encoding="ascii") as f:
                f.write(self.password + "\n")
            cmd = self.run([self.bin(c, "initdb"), "-D", c.get("data_arg", c["data"]), "--encoding=UTF8", *c.get("locale_flags", ["--locale=C"]),
                            "--auth=scram-sha-256", "--username=" + USER, "--pwfile=" + str(c.get("password_arg", pw))])
            details.update({"initdb_seconds": cmd["seconds"], "data_dir": str(c["data"]), "data_bytes": tree_size(c["data"])})
            c["initialized"] = True
        finally:
            pw.unlink(missing_ok=True)

    def pid_info(self, c):
        lines = self.decode((c["data"] / "postmaster.pid").read_bytes(), c, "pid_file").splitlines()
        pid = int(lines[0])
        if pid <= 1 or not contained(Path(lines[1]), c["data"]) or Path(lines[1]).resolve() != c["data"].resolve():
            raise RuntimeError("Refusing unverified postmaster.pid")
        return pid, lines

    def identity(self, pid):
        if os.name == "nt":
            return windows_identity(pid)
        if platform.system() == "Linux":
            stat = Path(f"/proc/{pid}/stat").read_text()
            return (str(Path(f"/proc/{pid}/exe").resolve()), stat[stat.rfind(")") + 2:].split()[19])
        lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
        lib.proc_pidpath.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]
        lib.proc_pidpath.restype = ctypes.c_int
        buf = ctypes.create_string_buffer(4096)
        if lib.proc_pidpath(pid, buf, len(buf)) <= 0:
            raise OSError(ctypes.get_errno(), "proc_pidpath")
        raw = self.tool("ps", "-p", str(pid), "-o", "lstart=")["stdout"].strip()
        if not raw:
            raise RuntimeError("Postmaster no longer exists")
        return (str(Path(os.fsdecode(buf.value)).resolve()), raw)

    def owns(self, c):
        if c["pid"] is None or c["identity"] is None:
            return False
        try:
            return self.identity(c["pid"]) == c["identity"]
        except Exception:
            return False

    def capture(self, c):
        pid, lines = self.pid_info(c)
        ident = self.identity(pid)
        binary = str(self.bin(c, "postgres").resolve())
        if isinstance(ident, tuple):
            if os.path.normcase(ident[0]) != os.path.normcase(binary):
                raise RuntimeError(f"PID {pid} does not run the private postgres binary")
        elif binary not in ident:
            raise RuntimeError(f"PID {pid} executable path not verified: {ident}")
        c["pid"], c["identity"] = pid, ident
        return lines

    def remember_descendants(self, c):
        if os.name == "nt":
            links = [(r["pid"], r["parent"]) for r in windows_processes()]
        else:
            raw = self.tool("ps", "-ax", "-o", "pid=,ppid=")["stdout"]
            links = [tuple(map(int, row.split())) for row in raw.splitlines() if len(row.split()) == 2]
        owned = {c["pid"]}
        while True:
            expanded = owned | {pid for pid, parent in links if parent in owned}
            if expanded == owned:
                break
            owned = expanded
        binary = os.path.normcase(str(self.bin(c, "postgres").resolve()))
        for pid in owned - {c["pid"]}:
            try:
                ident = self.identity(pid)
                if os.path.normcase(ident[0]) == binary:
                    c["descendants"][pid] = ident
            except (OSError, RuntimeError):
                pass  # A backend may have already exited between enumeration/query.

    def descendant_alive(self, pid, ident):
        try:
            return self.identity(pid) == ident
        except Exception:
            return False

    def wait_descendants(self, c, details, force=False):
        deadline = self.bounded_deadline(15)
        live = {}
        while True:
            live = {pid: ident for pid, ident in c["descendants"].items() if self.descendant_alive(pid, ident)}
            if not live or time.monotonic() >= deadline:
                break
            time.sleep(0.1)
        if live and force:
            for pid, ident in live.items():
                if self.descendant_alive(pid, ident):
                    if os.name == "nt":
                        windows_terminate_owned(pid, ident, c["root"])
                    else:
                        os.kill(pid, signal.SIGKILL)
            # Workers observed before shutdown only; never a machine-wide name kill.
            time.sleep(0.5)
            live = {pid: ident for pid, ident in live.items() if self.descendant_alive(pid, ident)}
        details["remaining_tracked_workers"] = list(live)
        if live:
            raise RuntimeError(f"Tracked private workers remain after postmaster exit: {list(live)}")

    def start(self, c, details):
        if not c["initialized"]:
            raise RuntimeError("Prerequisite unavailable: successful initdb")
        if self.owns(c):
            details["already_running"] = True
            ready = self.run([self.bin(c, "pg_isready"), "-h", "127.0.0.1", "-p", str(c["port"]), "-U", USER, "-d", "postgres"])
            query = self.psql(c, "SELECT 1")
            if query["stdout"].strip() != "1":
                raise RuntimeError("Authenticated TCP readiness query did not return 1")
            details["tcp"] = {"status": "pass", "seconds": ready["seconds"] + query["seconds"], "server_ready": True, "authenticated": True}
            return
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
            s.bind(("127.0.0.1", 0))
            c["port"] = s.getsockname()[1]
        # PostgreSQL configuration literals, not shell/pg_ctl -o quoting. Windows
        # paths use forward slashes; apostrophes in the work path are doubled.
        def literal(s):
            return "'" + str(s).replace("\\", "/").replace("'", "''") + "'"
        sock = "" if os.name == "nt" else str(c["socket"])
        socket_path = c["socket"] / f".s.PGSQL.{c['port']}"
        # sockaddr_un includes a NUL; macOS is the smaller common desktop limit.
        socket_too_long = os.name != "nt" and len(os.fsencode(socket_path)) >= (104 if platform.system() == "Darwin" else 108)
        socket_setup_error = None
        if os.name != "nt" and not socket_too_long:
            try:
                c["socket"].mkdir(exist_ok=True)
                c["socket"].chmod(0o700)
            except OSError as e:
                socket_setup_error = str(e)
        if socket_too_long or socket_setup_error:
            sock = ""
        cfg = ("\n# private desktop spike settings\nlisten_addresses = '127.0.0.1'\n"
               f"port = {c['port']}\nshared_buffers = '64MB'\nmax_connections = 20\n"
               f"unix_socket_directories = {literal(sock)}\npassword_encryption = 'scram-sha-256'\n"
               "logging_collector = off\nlog_destination = 'stderr'\n")
        with (c["data"] / "postgresql.conf").open("a", encoding="utf-8") as f:
            f.write(cfg)
        c["start_attempted"] = True
        c["stop_verified"] = False
        try:
            cmd = self.run([self.bin(c, "pg_ctl"), "-D", c.get("data_arg", c["data"]), "-l", c.get("log_arg", c["log"]), "-w", "-t", "60", "start"], timeout=90)
            details.update({"start_seconds": cmd["seconds"], "port": c["port"], "postmaster_pid": self.capture(c)})
        finally:
            # pg_ctl can time out or be interrupted after spawning postgres.
            if not self.owns(c) and (c["data"] / "postmaster.pid").exists():
                self.capture(c)
        ready = self.run([self.bin(c, "pg_isready"), "-h", "127.0.0.1", "-p", str(c["port"]), "-U", USER, "-d", "postgres"])
        authenticated = self.psql(c, "SELECT 1")
        if authenticated["stdout"].strip() != "1":
            raise RuntimeError("Authenticated TCP readiness query did not return 1")
        details["tcp"] = {"status": "pass", "seconds": ready["seconds"] + authenticated["seconds"],
                          "server_ready": True, "authenticated": True}
        try:
            self.remember_descendants(c)
        except Exception as e:
            details["worker_enumeration_error"] = str(e)
        if os.name == "nt":
            details["unix_socket"] = {"status": "not-applicable", "seconds": 0, "applicable": False, "reason": "Windows: Unix socket probe inapplicable",
                                      "path_bytes": len(os.fsencode(socket_path)), "platform_limit_bytes": None}
        elif socket_too_long:
            details["unix_socket"] = {"status": "skipped-path-too-long", "seconds": 0, "path": str(socket_path),
                                      "path_bytes": len(os.fsencode(socket_path)), "platform_limit_bytes": 104 if platform.system() == "Darwin" else 108,
                                      "reason": "sockaddr_un requires a terminating NUL; TCP startup tested independently"}
        elif socket_setup_error:
            details["unix_socket"] = {"status": "fail", "seconds": 0, "path": str(socket_path),
                                      "path_bytes": len(os.fsencode(socket_path)), "platform_limit_bytes": 104 if platform.system() == "Darwin" else 108,
                                      "error": socket_setup_error}
        else:
            def socket_probe(d):
                d.update({"path": str(socket_path), "path_bytes": len(os.fsencode(socket_path)),
                          "platform_limit_bytes": 104 if platform.system() == "Darwin" else 108})
                probe = self.psql(c, "SELECT 1", host=str(c["socket"]), check=False)
                d["command"] = probe
                if probe["returncode"] != 0 or probe["stdout"].strip() != "1":
                    raise RuntimeError("Unix socket query failed: " + probe["stdout"] + probe["stderr"])
            probe = self.subcheck(details, "unix_socket", socket_probe)
            probe.update(probe.pop("details"))
        before_memory = time.perf_counter()
        details["idle_memory"] = self.memory(c)
        details["idle_memory"].update({"status": "fail" if "error" in details["idle_memory"] else "pass",
                                       "seconds": time.perf_counter() - before_memory})

    def memory(self, c):
        try:
            if os.name == "nt":
                # Get-Process WorkingSet64 is resident working set, not commit size.
                cmd = self.tool("powershell", "-NoProfile", "-NonInteractive", "-Command", f"(Get-Process -Id {c['pid']} -ErrorAction Stop).WorkingSet64")
                return {"postmaster_rss_bytes": int(cmd["stdout"].strip()), "method": "Get-Process WorkingSet64; postmaster only"}
            cmd = self.tool("ps", "-p", str(c["pid"]), "-o", "rss=")
            result = {"postmaster_rss_bytes": int(cmd["stdout"].strip()) * 1024, "method": "ps rss KiB; sum includes shared pages more than once, not unique physical memory"}
            rows = self.tool("ps", "-ax", "-o", "pid=,ppid=,rss=")["stdout"].splitlines()
            parsed = [tuple(map(int, row.split())) for row in rows if len(row.split()) == 3]
            owned = {c["pid"]}
            while True:
                expanded = owned | {pid for pid, parent, rss in parsed if parent in owned}
                if expanded == owned:
                    break
                owned = expanded
            result["process_tree_rss_sum_bytes"] = sum(rss * 1024 for pid, parent, rss in parsed if pid in owned)
            result["process_count"] = len(owned)
            return result
        except Exception as e:
            return {"error": str(e), "postmaster_rss_bytes": None}

    def psql(self, c, sql, database="postgres", password=True, host="127.0.0.1", check=True):
        if not self.owns(c):
            raise RuntimeError("Private server is not running or its identity changed")
        return self.run([self.bin(c, "psql"), "-X", "-w", "-h", host, "-p", str(c["port"]), "-U", USER,
                         "-d", database, "-v", "ON_ERROR_STOP=1", "-A", "-t", "-q", "-f", "-"],
                        input_bytes=(sql + "\n").encode("utf-8"), password=password, check=check)

    def locale_samples(self, c, database, details, extra=False):
        details["ascii_key_trigrams"] = self.psql(c, "SELECT show_trgm('u30eau30bcu30ed')::text", database=database)["stdout"].strip()
        details["ascii_key_nonempty"] = details["ascii_key_trigrams"] not in ("", "{}")
        details["simple_tsvector"] = self.psql(c, "SELECT to_tsvector('simple', 'リゼロ から Ōkami')::text", database=database)["stdout"].strip()
        tokens = self.psql(c, "SELECT tsvector_to_array(to_tsvector('simple', 'リゼロ から Ōkami')) "
            "@> ARRAY['リゼロ', 'から'] AND tsvector_to_array(to_tsvector('simple', 'リゼロ から Ōkami')) "
            "&& ARRAY['Ōkami', 'ōkami']", database=database)["stdout"].strip()
        details["simple_keeps_non_ascii_tokens"] = tokens == "t"
        samples = details["samples"] = {}
        for name, title in (("kana", "リゼロ"), ("cyrillic", "Привет"),
                            ("hangul", "나혼자"), ("latin", "Ōkami")):
            # Multibyte trigrams are hashed by pg_trgm, so compare the letter's
            # contribution against the ASCII suffix, rather than literal Ō text.
            sql = f"SELECT show_trgm('{title}')::text, cardinality(show_trgm('{title}')) > 0"
            if name == "latin":
                sql += ", EXISTS (SELECT unnest(show_trgm('Ōkami')) EXCEPT SELECT unnest(show_trgm('kami')))"
            row = self.psql(c, sql, database=database)["stdout"].strip().split("|")
            samples[name] = {"trigrams": row[0], "nonempty": row[1] == "t"}
            if name == "latin":
                samples[name]["non_ascii_letter_trigram"] = row[2] == "t"
        if extra:
            details["sort_result"] = self.psql(c, "SELECT string_agg(x, ',' ORDER BY x) FROM "
                "(VALUES ('a'),('B'),('Z'),('é'),('_')) v(x)", database=database)["stdout"].strip()
            details["byte_order"] = details["sort_result"] == "B,Z,_,a,é"
            details["lower_result"] = self.psql(c, "SELECT lower('ÄB ŌKAMI')", database=database)["stdout"].strip()
            details["kana_alnum"] = self.psql(c, "SELECT 'リゼロ' ~ '^[[:alnum:]]+$'", database=database)["stdout"].strip() == "t"

    def locale_probe(self, c, details):
        details["role"] = "observation; libc classification is not a product gate"
        default = details["cluster_default"] = {}
        self.subcheck(default, "observations", lambda d: self.locale_default(c, d))
        candidates = details["candidates"] = []
        first = None
        for locale in LC_CTYPE_CANDIDATES:
            rec = self.locale_candidate(c, locale, "C.UTF-8")
            candidates.append(rec)
            if rec["status"] == "pass" and first is None:
                first = locale
        details["first_working_lc_ctype"] = first
        created = next((r["name"] for r in candidates if r["created"]), None)
        details["first_created_lc_ctype"] = created
        # Explicit C classification comparison as well as the cluster default.
        details["lc_ctype_c"] = self.locale_candidate(c, "C", "C.UTF-8")
        if created:
            details["pg_unicode_fast"] = self.locale_candidate(c, first or created, "PG_UNICODE_FAST")
        baseline = default.get("observations", {}).get("details", {}).get("ascii_key_trigrams")
        comparisons = candidates + [details["lc_ctype_c"]] + ([details["pg_unicode_fast"]] if created else [])
        for rec in comparisons:
            if "ascii_key_trigrams" in rec:
                rec["ascii_key_matches_cluster_default"] = bool(baseline) and rec["ascii_key_trigrams"] == baseline
            if rec.get("created") and any(not rec.get("samples", {}).get(n, {}).get("nonempty", True) for n in ("kana", "hangul")):
                rec["finding"] = "LC_CTYPE does not produce kana/Hangul trigrams"
        measured = [r for r in comparisons if r.get("created")]
        details["ascii_key_identical_under_measured_lc_ctype"] = bool(measured) and bool(baseline) and all(
            r.get("ascii_key_nonempty") and r.get("ascii_key_matches_cluster_default") for r in measured)
        if first:
            if "windows" in self.triple:
                details["initdb"] = {"status": "pending", "reason": "Awaiting remaining Windows matrix budget"}
            else:
                self.subcheck(details, "initdb", lambda d: self.locale_initdb(c, first, d))
        else:
            details["initdb"] = {"status": "skipped", "reason": "No working LC_CTYPE"}
            details["finding"] = "No measured LC_CTYPE supports all Unicode trigram samples; recorded observation"

    def locale_default(self, c, details):
        details["lc_ctype"] = self.psql(c, "SELECT datctype FROM pg_database WHERE datname = current_database()")["stdout"].strip()
        row = self.psql(c, "SELECT datlocprovider, datlocale, datcollate, datctype "
            "FROM pg_database WHERE datname = current_database()")["stdout"].strip().split("|")
        details["database_locale"] = dict(zip(("datlocprovider", "datlocale", "datcollate", "datctype"), row))
        self.locale_samples(c, "postgres", details)

    def locale_candidate(self, c, locale, builtin):
        rec = {"name": locale, "builtin_locale": builtin, "created": False, "status": "fail"}
        database = "spike_ctype_" + secrets.token_hex(12)
        rec["database"] = database
        try:
            created = self.psql(c, f"CREATE DATABASE {database} TEMPLATE template0 ENCODING 'UTF8' "
                f"LOCALE_PROVIDER builtin BUILTIN_LOCALE '{builtin}' LC_COLLATE 'C' LC_CTYPE '{locale}'", check=False)
            if created["returncode"]:
                rec["error"] = created["stdout"] + created["stderr"]
            else:
                rec["created"] = True
                self.psql(c, "CREATE EXTENSION pg_trgm", database=database)
                self.locale_samples(c, database, rec, extra=True)
                if (all(v["nonempty"] for v in rec["samples"].values())
                        and rec["samples"]["latin"]["non_ascii_letter_trigram"] and rec["byte_order"]):
                    rec["status"] = "pass"
        except Exception as e:
            rec["error"] = self.redact(str(e))
        finally:
            if rec["created"]:
                try:
                    dropped = self.psql(c, f"DROP DATABASE {database}", check=False)
                    rec["dropped"] = dropped["returncode"] == 0
                    if not rec["dropped"]:
                        rec["cleanup_error"] = dropped["stdout"] + dropped["stderr"]
                except Exception as e:
                    rec["dropped"] = False
                    rec["cleanup_error"] = self.redact(str(e))
        return rec

    def locale_initdb(self, c, locale, details):
        probe = self.new_cluster(c["root"], "lc-ctype-" + secrets.token_hex(8))
        probe["data"] = self.work / ("lc-ctype-" + secrets.token_hex(8)) / "pg18"
        probe["locale_flags"] = ["--locale-provider=builtin", "--builtin-locale=C.UTF-8",
                                 "--lc-collate=C", "--lc-ctype=" + locale]
        details["flags"] = probe["locale_flags"]
        self.init(probe, details)

    def queries(self, c, details):
        retained = self.subcheck(details, "retained_control_modules", lambda d: self.retained_module_loads(c, d))
        def module(d):
            self.psql(c, "CREATE EXTENSION IF NOT EXISTS pg_trgm; SELECT similarity('manga','manga');")
            d["pg_trgm_loaded"] = True
        loaded = self.subcheck(details, "pg_trgm", module)
        details["pg_trgm_loaded"] = loaded["status"] == "pass"
        def fixture(d):
            self.psql(c, "DROP TABLE IF EXISTS spike_books; "
                      "CREATE TABLE spike_books(id integer PRIMARY KEY, title text NOT NULL, words tsvector); "
                      "INSERT INTO spike_books VALUES (1,'manga adventures',to_tsvector('simple','manga adventures')), "
                      "(2,'cooking recipes',to_tsvector('simple','cooking recipes'));")
        prepared = self.subcheck(details, "search_fixture", fixture)
        def similarity(d):
            self.psql(c, "CREATE INDEX spike_title_trgm ON spike_books USING gin(title gin_trgm_ops)")
            similar = self.psql(c, "SELECT id FROM spike_books WHERE similarity(title,'manga adventure') > 0.3 ORDER BY id")["stdout"].strip()
            details["similarity_result"] = d["similarity_result"] = similar
            if similar != "1":
                raise RuntimeError("Similarity result mismatch: must return exactly id 1")
        def fulltext(d):
            self.psql(c, "CREATE INDEX spike_words ON spike_books USING gin(words)")
            fulltext = self.psql(c, "SELECT id FROM spike_books WHERE words @@ plainto_tsquery('simple','manga') ORDER BY id")["stdout"].strip()
            details["fulltext_result"] = d["fulltext_result"] = fulltext
            if fulltext != "1":
                raise RuntimeError("Full-text result mismatch: must return exactly id 1")
        if prepared["status"] == "pass":
            similar = self.subcheck(details, "similarity", similarity)
            fulltext_result = self.subcheck(details, "fulltext", fulltext)
        else:
            similar = details["similarity"] = {"status": "skipped-prerequisite", "reason": "Search fixture unavailable", "seconds": 0}
            fulltext_result = details["fulltext"] = dict(similar)
        def auth(d):
            refused = self.psql(c, "SELECT 1", password=False, check=False)
            details["without_password"] = refused
            # A generic connection error isn't proof that authentication was tested.
            text = refused["stdout"] + refused["stderr"]
            details["without_password_refused"] = refused["returncode"] != 0 and bool(re.search(r"no password supplied|password authentication failed", text, re.I))
            d["without_password_refused"] = details["without_password_refused"]
            if not details["without_password_refused"]:
                raise RuntimeError("Missing-password connection did not fail with a password error")
        authenticated = self.subcheck(details, "authentication", auth)
        if not hasattr(self, "lc_ctype_probe"):
            holder = {}
            self.lc_ctype_probe = self.subcheck(holder, "lc_ctype_probe", lambda d: self.locale_probe(c, d))
        details["lc_ctype_probe"] = self.lc_ctype_probe
        checks = [retained, loaded, prepared, similar, fulltext_result, authenticated]
        self.require_checks(*checks)

    def stop(self, c, details):
        if not self.owns(c):
            raise RuntimeError("No owned running postmaster to stop")
        try:
            self.remember_descendants(c)
        except Exception as e:
            details["worker_enumeration_error"] = str(e)
        cmd = self.run([self.bin(c, "pg_ctl"), "-D", c.get("data_arg", c["data"]), "-w", "-t", "60", "stop", "-m", "fast"], timeout=90)
        details["stop_seconds"] = cmd["seconds"]
        if self.owns(c):
            raise RuntimeError("Postmaster still alive after clean stop")
        c["pid"], c["identity"] = None, None
        self.wait_descendants(c, details)
        c["stop_verified"] = True

    def ensure_fixture(self, c):
        # Search/auth can fail without withholding the lifecycle test's data.
        # Never recreate this fixture after restart: that could hide data loss.
        if not c["fixture_seeded"]:
            self.psql(c, "CREATE TABLE spike_persistence(id integer PRIMARY KEY, value text NOT NULL); "
                      "INSERT INTO spike_persistence VALUES (1,'before crash one'),(2,'before crash two'); "
                      "CREATE TABLE spike_crash_write(id integer PRIMARY KEY, value text NOT NULL);")
            c["fixture_seeded"] = True

    def check_rows(self, c, database="postgres"):
        rows = self.psql(c, "SELECT id, value FROM spike_persistence ORDER BY id", database=database)["stdout"].strip()
        if rows != "1|before crash one\n2|before crash two":
            raise RuntimeError(f"Preserved row contents differ: {rows!r}")
        return "2"

    def cycle(self, c, details):
        fixture = self.subcheck(details, "fixture", lambda d: self.ensure_fixture(c))
        stopped = self.subcheck(details, "stop", lambda d: self.stop(c, d))
        restarted = self.subcheck(details, "restart", lambda d: self.start(c, d))
        def preserved(d):
            details["preserved_rows"] = d["preserved_rows"] = self.check_rows(c)
        if c["fixture_seeded"]:
            rows = self.subcheck(details, "persistence", preserved)
        else:
            rows = details["persistence"] = {"status": "skipped-prerequisite", "reason": "Lifecycle fixture unavailable", "seconds": 0}
        self.require_checks(fixture, stopped, restarted, rows)

    def hard_kill(self, c):
        if not self.owns(c):
            raise RuntimeError("Refusing to kill a postmaster whose PID identity changed")
        if os.name == "nt":
            # No /T: this crash experiment kills only the postmaster.
            windows_terminate_owned(c["pid"], c["identity"], c["root"])
        else:
            os.kill(c["pid"], signal.SIGKILL)

    def crash(self, c, details):
        errors = []
        try:
            if not self.owns(c):
                self.start(c, details.setdefault("pre_crash_start", {}))
            self.ensure_fixture(c)
            details["pre_crash_rows"] = self.check_rows(c)
            details["pid_before_kill"] = self.pid_info(c)[1]
            try:
                self.remember_descendants(c)
            except Exception as e:
                details["worker_enumeration_error"] = str(e)
            # One transaction, two distinct rows. Asynchronous commit permits
            # either the complete transaction or its complete absence on crash.
            self.psql(c, "TRUNCATE spike_crash_write")
            self.psql(c, "CHECKPOINT")
            write = self.psql(c, "BEGIN; SET LOCAL synchronous_commit = off; "
                              "INSERT INTO spike_crash_write VALUES (1,'just before kill one'),(2,'just before kill two'); COMMIT;")
            details["pre_kill_write"] = {"status": "pass", "seconds": write["seconds"], "rows_attempted": 2,
                                        "commit_acknowledged": True, "synchronous_commit": False}
            self.hard_kill(c)
            deadline = self.bounded_deadline(15)
            while self.owns(c) and time.monotonic() < deadline:
                time.sleep(0.1)
            if self.owns(c):
                raise RuntimeError("Hard-killed postmaster did not exit within 15 seconds")
            self.wait_descendants(c, details)
            # Give workers time to notice their dead postmaster; do not remove locks.
            time.sleep(2)
            pidfile = c["data"] / "postmaster.pid"
            details["stale_pid_before_restart"] = self.decode(pidfile.read_bytes(), details, "stale_pid") if pidfile.exists() else None
            details["lock_handling"] = "No PID file removed or modified by spike; pg_ctl/postgres handles it"
            details["recovery"] = {}
            before_recovery = time.perf_counter()
            self.start(c, details["recovery"])
            details["preserved_rows"] = self.check_rows(c)
            rows = self.psql(c, "SELECT id, value FROM spike_crash_write ORDER BY id")["stdout"].strip()
            outcome = "fully-absent" if rows == "" else ("fully-present" if rows == "1|just before kill one\n2|just before kill two" else "partial-or-corrupt")
            details["crash_write"] = {"status": "fail" if outcome == "partial-or-corrupt" else "pass", "outcome": outcome, "rows": rows}
            details["recovery_succeeded"] = details["recovery"]["tcp"]["server_ready"] and outcome != "partial-or-corrupt"
            if not details["recovery_succeeded"]:
                raise RuntimeError("Crash write was only partly present or had changed contents")
            details["recovery_check"] = {"status": "pass", "seconds": time.perf_counter() - before_recovery,
                                         "preserved_rows": details["preserved_rows"], "outcome": outcome, "server_ready": True}
        except Exception as e:
            details["recovery_succeeded"] = False
            details["recovery_check"] = {"status": "fail", "error": str(e)}
            errors.append(str(e))
        # Duplicate starts are observations, independent of recovery acceptance.
        if self.owns(c):
            self.subcheck(details, "second_start_check", lambda d: self.live_start_probe(c, d, direct=False))
            second = details["second_start_check"]["details"]
            details["second_start_refused"] = second.get("refused", False)
            if "command" in second:
                details["second_start"] = second["command"]
            self.subcheck(details, "direct_postgres_start_check", lambda d: self.live_start_probe(c, d, direct=True))
        if errors:
            raise RuntimeError("\n".join(errors))

    def live_start_probe(self, c, details, direct=False):
        original_pid, original_identity = c["pid"], c["identity"]
        if not self.owns(c):
            raise RuntimeError("Live-directory probe needs an identified running server")
        args = [self.bin(c, "postgres" if direct else "pg_ctl"), "-D", c.get("data_arg", c["data"])]
        if not direct:
            # Deliberately retain the live log: Windows pg_ctl can block in its
            # log-sharing retry BEFORE -t's readiness loop. Bound the whole probe.
            args += ["-l", c.get("log_arg", c["log"]), "-w", "-t", "3", "start"]
        command_start = len(self.result["commands"])
        try:
            command = self.run(args, check=False, timeout=5, probe_cluster=c)
            text = command["stdout"] + command["stderr"]
            # pg_ctl's warning alone is NOT evidence of lock-file refusal.
            details["refused"] = command["returncode"] != 0 and bool(re.search(
                r"already running|lock file.*already exists", text, re.I))
            details["command"] = command
            details["exit_code"] = command["returncode"]
            details["message"] = text
        except subprocess.TimeoutExpired as e:
            details.update(refused="timeout", exit_code=None, message=str(e))
            if len(self.result["commands"]) > command_start:
                details["command"] = self.result["commands"][-1]
        finally:
            details["original_server_unchanged"] = self.owns(c) and (c["pid"], c["identity"]) == (original_pid, original_identity)
        if not details["original_server_unchanged"]:
            raise RuntimeError("Live-directory probe changed the recovered server identity")

    def backup(self, c, details):
        if not self.owns(c):
            self.start(c, details.setdefault("pre_dump_start", {}))
        self.ensure_fixture(c)
        dump = c.get("dump_file", self.work / "nightly-test.dump")
        dump_arg = c.get("dump_arg", dump)
        common = ["-h", "127.0.0.1", "-p", str(c["port"]), "-U", USER, "-w"]
        def dumping(d):
            cmd = self.run([self.bin(c, "pg_dump"), *common, "-d", "postgres", "-Fc", "-f", dump_arg], password=True)
            details.update({"dump_seconds": cmd["seconds"], "dump_bytes": dump.stat().st_size})
            d["dump_bytes"] = details["dump_bytes"]
        dumped = self.subcheck(details, "dump", dumping)
        if dumped["status"] == "fail":
            details["restore"] = {"status": "skipped-prerequisite", "reason": "No successful dump", "seconds": 0}
            self.require_checks(dumped)
        def restoring(d):
            self.psql(c, "CREATE DATABASE spike_restored")
            restore = self.run([self.bin(c, "pg_restore"), *common, "--exit-on-error", "-d", "spike_restored", dump_arg], password=True)
            details["restore_seconds"] = d["restore_seconds"] = restore["seconds"]
        restored = self.subcheck(details, "restore", restoring)
        if restored["status"] == "pass":
            def verify(d):
                details["restored_rows"] = d["restored_rows"] = self.check_rows(c, "spike_restored")
            verified = self.subcheck(details, "restored_data", verify)
            self.require_checks(verified)
        else:
            details["restored_data"] = {"status": "skipped-prerequisite", "reason": "No successful restore", "seconds": 0}
            self.require_checks(restored)

    def retained_module_loads(self, c, details):
        # Dependency inspection cannot prove dlopen in the actual backend context.
        # One connection per LOAD isolates extension _PG_init hooks from search.
        details["modules"] = []
        for check in getattr(self, "control_checks", []):
            if check["status"] != "present":
                continue
            module = check["module_pathname"]
            rec = {"control": check["control"], "module_pathname": module, "status": "fail"}
            details["modules"].append(rec)
            try:
                cmd = self.psql(c, "LOAD '" + module.replace("'", "''") + "'", check=False)
                rec["command"] = cmd
                if cmd["returncode"] != 0:
                    raise RuntimeError(cmd["stdout"] + cmd["stderr"])
                rec["status"] = "pass"
            except Exception as e:
                rec["error"] = self.redact(str(e))
                # Only a backend module-loading diagnostic proves incompatibility.
                # Timeouts, connection loss and unavailable ownership do not.
                proven = rec.get("command", {}).get("returncode", 0) != 0 and bool(re.search(
                    r"could not (?:load|access) (?:library|file)|incompatible library|missing magic block|must be loaded via shared_preload_libraries",
                    rec["error"], re.I))
                if proven and Path(check["control"]).stem not in ("pg_trgm", "plpgsql"):
                    # A proved unloadable optional extension must not advertise a
                    # usable control file. Retain other libraries conservatively.
                    name = Path(check["control"]).stem
                    dropped = [n for n in self.runtime_files if n == check["control"] or
                               (PurePosixPath(n).name.startswith(name + "--") and n.endswith(".sql"))]
                    for n in dropped:
                        (self.runtime / n).unlink()
                        if c["root"] != self.runtime:
                            (c["root"] / n).unlink(missing_ok=True)
                        self.runtime_files.remove(n)
                        finding = {"file": n, "reason": "declared module failed backend LOAD",
                                   "module_pathname": module, "error": rec["error"]}
                        details.setdefault("excluded_findings", []).append(finding)
                        for step in self.result["steps"]:
                            if step["step"] in (2, 3):
                                step["details"].setdefault("excluded_findings", []).append(dict(finding))
                                for control in step["details"].get("control_module_checks", []):
                                    if control["control"] == check["control"]:
                                        control["status"] = "excluded-unloadable-module"
                            if step["step"] == 2:
                                step["details"].setdefault("drop_files", []).append(n)
                                step["details"]["runtime_bytes"] = tree_size(self.runtime)
                    check["status"] = "excluded-unloadable-module"
                    rec["status"] = "excluded"
        failed = [r for r in details["modules"] if r["status"] == "fail"]
        if failed:
            raise RuntimeError(json.dumps(failed, ensure_ascii=False))

    def prepare_windows_trial(self, trial, index):
        c = self.new_cluster(self.runtime, f"win-{index:02d}")
        trial["cluster_created"] = True
        output_dir = self.work / f"win-command-{index:02d}"
        if not str(output_dir).isascii():
            raise RuntimeError("Windows command transcripts need an ASCII private work path")
        output_dir.mkdir()
        c["command_output_dir"] = self.command_output_dir = output_dir
        # Python always uses real absolute wide-character paths for ownership,
        # configuration, PID files and cleanup. Only tool arguments are aliased.
        suffix = {"A": "ASCII space", "B": "Western é", "C": "Unicode 漢字"}[trial["class"]]
        parent = self.work / f"win-{index:02d} {suffix}"
        parent.mkdir()
        c.update({"windows_trial": True, "data": parent / "pg18"})
        trial["real_parent"] = str(parent)
        strategy = trial["strategy"]
        alias = parent
        if strategy == "short":
            alias = windows_short_path(parent)
            trial["short_path"] = str(alias) if alias else None
            if alias is None:
                trial.update({"status": "skipped", "reason": "Volume did not provide an ASCII 8.3 path"})
                return c
        elif strategy == "junction":
            alias = self.work / f"win-link-{index:02d}"
            if not str(alias).isascii():
                trial.update({"status": "skipped", "reason": "Work directory cannot supply an ASCII junction path"})
                return c
            # Retain the path even if New-Item creates the link then errors.
            c["junction"] = alias
            windows_create_junction(alias, parent, self.tool)
            trial["junction"] = str(alias)
        if strategy == "relative":
            self.command_cwd = parent  # subprocess cwd uses Python's Unicode API.
            alias = Path(".")
        c["data_arg"] = alias / "pg18"
        helpers = parent
        helper_alias = alias
        if strategy == "ascii-helpers":
            helpers = self.work / f"win-helpers-{index:02d}"
            helpers.mkdir()
            helper_alias = helpers
            if not str(helpers).isascii():
                trial.update({"status": "skipped", "reason": "Work directory cannot supply ASCII helper paths"})
                return c
        for key, filename in (("password", "password.tmp"), ("log", "postgres.log"),
                              ("dump", "nightly.dump"), ("pgpass", "empty.pgpass")):
            real, arg = helpers / filename, helper_alias / filename
            if key == "log":
                c["log"] = real
            else:
                c[key + "_file"] = real
            c[key + "_arg"] = arg
        c["pgpass_file"].touch(exist_ok=False)
        self.env["PGPASSFILE"] = str(c["pgpass_arg"])
        self.env.update({"TMP": str(helper_alias), "TEMP": str(helper_alias), "TMPDIR": str(helper_alias)})
        trial["tool_paths"] = {key: str(c[key + "_arg"]) for key in ("data", "password", "log", "dump", "pgpass")}
        trial["cwd"] = str(self.command_cwd)
        c["tool_cwd"], c["tool_env"] = self.command_cwd, self.env.copy()
        return c

    def windows_observations(self, c, details):
        details["data_directory"] = self.psql(c, "SHOW data_directory")["stdout"].strip()
        if c.get("junction"):
            details["reported_path_kind"] = ("junction" if os.path.normcase(details["data_directory"].replace("\\", "/")) ==
                os.path.normcase(str(c["data_arg"]).replace("\\", "/")) else "target" if
                os.path.normcase(details["data_directory"].replace("\\", "/")) == os.path.normcase(str(c["data"]).replace("\\", "/")) else "other")
        status = self.run([self.bin(c, "pg_ctl"), "-D", c["data_arg"], "status"])
        details["pg_ctl_status"] = {"status": "pass", "command": status, "data_arg": str(c["data_arg"])}

    def windows_relocated_junction_restart(self, c, details):
        self.stop(c, details.setdefault("stop", {}))
        old = c["junction"]
        new = old.with_name(old.name + "-fresh")
        details.update({"old_junction": str(old), "new_junction": str(new), "target": str(c["data"].parent)})
        windows_remove_junction(old)
        c["junction"] = new  # Track even partially successful creation for cleanup.
        windows_create_junction(new, c["data"].parent, self.tool)
        for key in ("data", "password", "log", "dump", "pgpass"):
            c[key + "_arg"] = new / c[key + "_arg"].name
        self.env.update({"PGPASSFILE": str(c["pgpass_arg"]), "TMP": str(new), "TEMP": str(new), "TMPDIR": str(new)})
        c["tool_cwd"], c["tool_env"] = self.command_cwd, self.env.copy()
        self.start(c, details.setdefault("restart", {}))
        self.subcheck(details, "path_observations", lambda d: self.windows_observations(c, d))
        details["preserved_rows"] = self.check_rows(c)

    def windows_junction_locations(self, details):
        details["locations"] = []
        # Never elevate: impersonate a filtered linked UAC token when available.
        target = self.work / ("junction-location-target-" + secrets.token_hex(8))
        target.mkdir()
        try:
            for variable in ("LOCALAPPDATA", "ProgramData"):
                rec = {"name": variable, "status": "fail", "standard_user_token": False}
                details["locations"].append(rec)
                base = next((v for k, v in os.environ.items() if k.lower() == variable.lower()), None)
                if not base:
                    rec.update({"status": "skipped", "reason": "Runner environment variable absent"})
                    continue
                link = Path(base) / ("komika-pg-spike-" + secrets.token_hex(12))
                rec["path"] = str(link)
                try:
                    windows_standard_junction_probe(link, target, rec)
                    rec["status"] = "pass"
                except Exception as e:
                    rec["error"] = str(e)
                    if not rec["standard_user_token"]:
                        rec["status"] = "unmeasured-no-standard-token"
                finally:
                    try:
                        if link.exists() or link.is_symlink():
                            windows_remove_junction(link)
                            rec["removed"] = True
                    except Exception as e:
                        rec.update({"status": "fail", "cleanup_error": str(e)})
                        self.result.setdefault("junction_cleanup_pending", []).append(str(link))
                rec["standard_user_creation_demonstrated"] = rec["standard_user_token"] and rec["status"] == "pass"
        finally:
            target.rmdir()

    def windows_different_cwd_restart(self, c, trial, details):
        reported = trial["stages"]["path-observations"]["details"].get("data_directory")
        self.stop(c, details.setdefault("stop", {}))
        old_cwd, old_arg, old_env = self.command_cwd, c["data_arg"], self.env.copy()
        try:
            self.command_cwd = self.work
            # Relative spelling is meaningful only at the original parent. Test
            # the known absolute wide path from a different working directory.
            # SHOW data_directory is diagnostic text, not a filesystem authority:
            # run 4's captured B/relative SHOW result had already lost é.
            c["data_arg"] = c["data"]
            details["reported_data_directory"] = reported
            details["reported_path_matches_real"] = os.path.normcase((reported or "").replace("\\", "/")) == os.path.normcase(str(c["data"]).replace("\\", "/"))
            # Startup log follows the new absolute spelling of the same parent.
            old_log = c["log_arg"]
            c["log_arg"] = c["log"]
            parent = c["data"].parent
            self.env.update({"PGPASSFILE": str(parent / "empty.pgpass"), "TMP": str(parent),
                             "TEMP": str(parent), "TMPDIR": str(parent)})
            details.update({"cwd": str(self.command_cwd), "data_arg": str(c["data_arg"]),
                            "pgpass_arg": self.env["PGPASSFILE"], "log_arg": str(c["log_arg"])})
            self.start(c, details.setdefault("restart", {}))
            self.subcheck(details, "path_observations", lambda d: self.windows_observations(c, d))
            details["preserved_rows"] = self.check_rows(c)
        finally:
            self.command_cwd, c["data_arg"], self.env = old_cwd, old_arg, old_env
            if "old_log" in locals():
                c["log_arg"] = old_log

    def windows_lifecycle(self, c, trial):
        stages = [
            ("initdb", lambda d: self.init(c, d)),
            ("start", lambda d: self.start(c, d)),
            ("path-observations", lambda d: self.windows_observations(c, d)),
            ("search-auth-modules", lambda d: self.queries(c, d)),
            ("persistence-fixture", lambda d: self.ensure_fixture(c)),
            ("fast-stop", lambda d: self.stop(c, d)),
            ("restart", lambda d: self.start(c, d)),
            ("persistence", lambda d: d.update(preserved_rows=self.check_rows(c))),
        ]
        if trial["strategy"] == "relative":
            stages.append(("restart-different-cwd", lambda d: self.windows_different_cwd_restart(c, trial, d)))
        if trial["strategy"] == "junction":
            stages.append(("restart-fresh-junction", lambda d: self.windows_relocated_junction_restart(c, d)))
        stages += [("crash-recovery", lambda d: self.crash(c, d)),
                   ("dump-restore", lambda d: self.backup(c, d)),
                   ("final-path-observations", lambda d: self.windows_observations(c, d)),
                   ("final-stop", lambda d: self.stop(c, d))]
        trial["stages"] = {}
        for name, fn in stages:
            trial["stage_reached"] = name
            rec = self.subcheck(trial["stages"], name, fn)
            self.persist()
            if rec["status"] != "pass" and name not in ("path-observations", "final-path-observations"):
                raise RuntimeError(rec["error"])
        trial["data_directory"] = trial["stages"]["path-observations"]["details"].get("data_directory")
        trial["pg_ctl_status_works"] = trial["stages"]["final-path-observations"]["status"] == "pass"
        trial["pg_dump_works"] = trial["stages"]["dump-restore"]["details"]["dump"]["status"] == "pass"
        trial["pg_restore_works"] = trial["stages"]["dump-restore"]["details"]["restore"]["status"] == "pass"
        trial["stage_reached"] = "complete"

    def windows_matrix(self, details):
        details.update({"applicable": True, "trial_cap": WINDOWS_TRIAL_CAP,
                        "matrix_budget_seconds": WINDOWS_MATRIX_SECONDS,
                        "trial_budget_seconds": WINDOWS_TRIAL_SECONDS,
                        "subst": {"status": "skipped", "reason": "Drive-letter mappings are shared runner state; collision-free ownership/removal cannot be guaranteed without a runner-wide lock"},
                        "trials": []})
        details["code_pages"] = windows_code_pages()
        self.subcheck(details, "junction_locations", self.windows_junction_locations)
        end = min(time.monotonic() + WINDOWS_MATRIX_SECONDS, self.started_monotonic + 960)
        # Interleave B/C and prioritize the portable ASCII junction workaround.
        plan = [("A", "direct")] + [(cls, strategy) for strategy in
                ("direct", "junction", "short", "relative", "ascii-helpers") for cls in ("C", "B")]
        for index, (cls, strategy) in enumerate(plan, 1):
            trial = {"name": f"{cls} / {strategy}", "class": cls, "strategy": strategy,
                     "status": "fail", "stage_reached": "setup", "stages": {}}
            details["trials"].append(trial)
            before = time.monotonic()
            first_command = len(self.result["commands"])
            if index > WINDOWS_TRIAL_CAP or end - before < WINDOWS_TRIAL_SECONDS:
                trial.update({"status": "skipped", "reason": "Trial cap or remaining matrix/job budget", "seconds": 0})
                continue
            old_env, old_cwd, old_deadline = self.env.copy(), self.command_cwd, self.command_deadline
            old_output_dir = self.command_output_dir
            c = None
            cluster_count = len(self.clusters)
            try:
                self.command_deadline = min(end, before + WINDOWS_TRIAL_SECONDS)
                c = self.prepare_windows_trial(trial, index)
                if trial["status"] != "skipped":
                    self.windows_lifecycle(c, trial)
                    trial["status"] = "pass"
            except Exception as e:
                trial["status"] = "fail"
                trial["error"] = self.redact(f"{type(e).__name__}: {e}")
            finally:
                # Setup may fail after registering a cluster or making a junction.
                if c is None and len(self.clusters) > cluster_count:
                    c = self.clusters[-1]
                try:
                    self.command_deadline = time.monotonic() + 40
                    if c is not None:
                        tracked = self.clusters
                        try:
                            self.clusters = [c]
                            self.cleanup(trial.setdefault("cleanup", {}), final=False)
                        finally:
                            self.clusters = tracked
                        if c.get("junction_removed"):
                            trial["junction_removed_without_recursion"] = True
                        self.clusters.remove(c)  # Only fully cleaned clusters leave tracking.
                except Exception as e:
                    trial["status"] = "fail"
                    trial["cleanup_error"] = self.redact(str(e))
                finally:
                    self.env, self.command_cwd, self.command_deadline = old_env, old_cwd, old_deadline
                    self.command_output_dir = old_output_dir
                    trial["seconds"] = time.monotonic() - before
                    trial["command_range"] = {"start": first_command, "end": len(self.result["commands"])}
                    self.persist()
        details["class_A_passed"] = any(t["class"] == "A" and t["status"] == "pass" for t in details["trials"])
        details["class_C_strategy_passed"] = any(t["class"] == "C" and t["status"] == "pass" for t in details["trials"])
        details["outcome"] = "pass" if details["class_A_passed"] and details["class_C_strategy_passed"] else "fail"
        if details["outcome"] == "fail":
            raise RuntimeError("Windows matrix requires a complete class A lifecycle and at least one complete class C strategy; see every trial below")

    def diagnostic_lines(self):
        """Unabridged evidence after the compact table, also included in Markdown."""
        lines = []
        matrix = self.result.get("windows_path_matrix", {})
        if matrix.get("applicable"):
            lines += ["", "Windows path matrix (A: ASCII + space; B: é; C: 漢字)",
                      json.dumps(matrix.get("code_pages", {}), ensure_ascii=False),
                      "", "| Class | Strategy | Status | Stage reached | Error / skip reason |",
                      "| --- | --- | --- | --- | --- |"]
            for trial in matrix.get("trials", []):
                error = trial.get("error", trial.get("reason", ""))
                if trial.get("cleanup_error"):
                    error += " Cleanup: " + trial["cleanup_error"]
                lines.append(f"| {trial['class']} | {trial['strategy']} | {trial['status']} | {trial['stage_reached']} | "
                             + error.replace("|", "\\|").replace("\n", " ") + " |")
            lines += ["", "subst: " + matrix.get("subst", {}).get("reason", "not reached")]
            for trial in matrix.get("trials", []):
                if trial["status"] != "fail":
                    continue
                span = trial.get("command_range", {"start": 0, "end": 0})
                for command in self.result["commands"][span["start"]:span["end"]]:
                    if command.get("error") or command.get("returncode", 0) != 0:
                        lines += ["", trial["name"] + " — full failed command (including timeout output):",
                                  json.dumps(command, ensure_ascii=False, indent=2)]
        def visit(label, node):
            if not isinstance(node, dict):
                return
            for key in ("error", "cleanup_error"):
                if key in node:
                    lines.extend(["", label + " — full " + key + ":", str(node[key])])
            for key, value in node.items():
                if isinstance(value, dict):
                    visit(label + " / " + key, value)
                elif isinstance(value, list) and key not in ("files", "commands", "excluded_findings"):
                    for i, item in enumerate(value):
                        if isinstance(item, dict):
                            visit(label + " / " + str(item.get("name", item.get("file", i + 1))), item)
        for step in self.result["steps"]:
            probe = step.get("details", {}).get("lc_ctype_probe")
            if probe:
                lines += ["", f"Step {step['step']} — full LC_CTYPE matrix:",
                          json.dumps(probe, ensure_ascii=False, indent=2)]
            visit(f"Step {step['step']} {step['name']}", step)
            excluded = step.get("details", {}).get("excluded_findings", [])
            if excluded:
                lines += ["", f"Step {step['step']} — excluded findings (file, reason, dependencies):",
                          json.dumps(excluded, ensure_ascii=False, indent=2)]
        return [str(self.redact(line)) for line in lines]

    def signed(self, details):
        if platform.system() != "Darwin":
            details.update({"applicable": False, "reason": "macOS only"})
            return
        if self.runtime is None:
            raise RuntimeError("Prerequisite unavailable: runtime selection")
        details["variants"] = []
        failures = []
        variants = [("a-adhoc", "-", False), ("b-adhoc-disable-library-validation", "-", True),
                    ("c-team", self.sign_identity, False)]
        for name, identity, disable_validation in variants:
            v = {"name": name, "status": "fail", "identity": identity, "pg_trgm_loaded": None,
                 "signatures": [], "repeated_steps": []}
            details["variants"].append(v)
            before = time.perf_counter()
            if identity is None:
                v.update({"status": "skipped-no-identity", "reason": "Supply --sign-identity to test the team-signed bundle", "seconds": 0})
                continue
            try:
                dest = self.work / ("signed " + name + " runtime é")
                copy_runtime(self.runtime, self.runtime_files, dest)
                entitlements = self.work / ("signed " + name + "-entitlements.plist")
                if disable_validation:
                    with entitlements.open("xb") as f:
                        plistlib.dump({"com.apple.security.cs.disable-library-validation": True}, f)
                    v["entitlements"] = {"com.apple.security.cs.disable-library-validation": True}
                else:
                    v["entitlements"] = {}
                machs = [p for p in files(dest) if not p.is_symlink() and magic(p) in MACH]
                if not machs:
                    raise RuntimeError("Signed runtime contains no Mach-O files")
                # Inner libraries/modules first; executable entitlements only.
                machs.sort(key=lambda p: (p.parent == dest / "bin", -len(p.parts), p.as_posix()))
                for p in machs:
                    if not contained(p, dest) or not contained(p, self.work):
                        raise ValueError("Refusing to sign outside the private variant copy")
                    rec = {"file": p.relative_to(dest).as_posix()}
                    v["signatures"].append(rec)
                    opts = ["--entitlements", entitlements] if disable_validation and p.parent == dest / "bin" else []
                    try:
                        rec["sign"] = self.tool("codesign", "--force", "--options", "runtime", "--sign", identity, *opts, p, check=False)
                    except Exception as e:
                        rec["error"] = str(e)
                        if name == "c-team":
                            v.update({"status": "skipped-signing-unavailable", "error": str(e),
                                      "reason": "codesign could not complete with the supplied identity; runtime sequence not run"})
                            break
                        raise
                    if rec["sign"]["returncode"] != 0:
                        error = rec["sign"]["stdout"] + rec["sign"]["stderr"]
                        if name == "c-team":
                            v.update({"status": "skipped-signing-unavailable", "error": error,
                                      "reason": "codesign could not use the supplied identity; runtime sequence not run"})
                            break
                        raise RuntimeError("codesign failed: " + error)
                    rec["verify"] = self.tool("codesign", "--verify", "--deep", "--strict", p)
                    rec["display"] = self.tool("codesign", "-dv", "--verbose=4", p)
                    raw = rec["display"]["stdout"] + rec["display"]["stderr"]
                    teams = re.findall(r"^TeamIdentifier=(.*)$", raw, re.M)
                    rec["team_id"] = teams[0] if teams else None
                if v["status"] == "skipped-signing-unavailable":
                    continue
                v["team_ids"] = sorted({r["team_id"] for r in v["signatures"] if r["team_id"] is not None})
                v["team_id"] = v["team_ids"][0] if len(v["team_ids"]) == 1 else None
                if name == "c-team" and (not v["team_id"] or v["team_id"] == "not set"
                                         or any(r["team_id"] != v["team_id"] for r in v["signatures"])):
                    raise RuntimeError("All Mach-O files must have the same nonempty Team ID")
                v["signed_runtime_bytes"] = tree_size(dest)
                c = self.new_cluster(dest, "signed " + name)
                for phase, fn in [("initdb", self.init), ("start", self.start), ("queries/auth", self.queries),
                                  ("stop/restart", self.cycle), ("crash-recovery", self.crash), ("final-stop", self.stop)]:
                    if phase != "initdb" and (not c["initialized"] or (phase != "start" and not self.owns(c))):
                        sub = {"name": phase, "status": "skipped-prerequisite", "seconds": 0,
                               "reason": "No initialized cluster / owned running server", "details": {}}
                    else:
                        holder = {}
                        sub = self.subcheck(holder, phase, lambda d, fn=fn: fn(c, d))
                        sub["name"] = phase
                    v["repeated_steps"].append(sub)
                    if phase == "queries/auth" and "pg_trgm_loaded" in sub["details"]:
                        v["pg_trgm_loaded"] = sub["details"]["pg_trgm_loaded"]
                failed = [r for r in v["repeated_steps"] if r["status"] == "fail"]
                # Only the observed dyld library-validation diagnostic is expected.
                # Signing failures or other runtime errors are still real failures.
                expected = (name == "a-adhoc" and len(failed) == 1 and failed[0]["name"] == "initdb"
                            and re.search(r"Library not loaded:.*(?:different Team IDs|library validation)", failed[0]["error"], re.S | re.I))
                if expected:
                    v.update({"status": "expected-failure", "finding": "ad-hoc hardened runtime fails library validation",
                              "error": failed[0]["error"]})
                elif failed or any(r["status"].startswith("skipped") for r in v["repeated_steps"]):
                    raise RuntimeError("\n".join(r.get("error", r.get("reason", "")) for r in v["repeated_steps"] if r["status"] != "pass"))
                else:
                    v["status"] = "pass"
            except Exception as e:
                v["error"] = self.redact(str(e))
                failures.append(name + ": " + v["error"])
            finally:
                v["seconds"] = time.perf_counter() - before
                self.persist()
        if failures:
            raise RuntimeError("\n".join(failures))

    def cleanup_identity(self, pid, details):
        # Do not interpret arbitrary API/encoding failures as proof of exit.
        for attempt in range(3):
            try:
                if os.name == "nt" and not windows_process_alive(pid):
                    return None
                return self.identity(pid)  # Windows image path is QueryFullProcessImageNameW.
            except (FileNotFoundError, ProcessLookupError):
                return None
            except Exception as e:
                details.setdefault("identity_check_errors", []).append(str(e))
                if attempt == 2:
                    raise
                time.sleep(0.1)

    def cleanup(self, details, final=True):
        failures = []
        unresolved = []
        details["clusters"] = []
        for c in reversed(self.clusters):
            rec = {"data_dir": str(c["data"]), "status": "fail"}
            before = time.perf_counter()
            details["clusters"].append(rec)
            old_cwd, old_env, old_deadline = self.command_cwd, self.env, self.command_deadline
            old_output_dir = self.command_output_dir
            self.command_output_dir = c.get("command_output_dir", old_output_dir)
            self.command_cwd = c.get("tool_cwd", old_cwd)
            self.env = c.get("tool_env", old_env)
            # Expired trial budgets must never prevent a stop attempt.
            self.command_deadline = time.monotonic() + 40
            try:
                pid = c["pid"]
                if c["start_attempted"]:
                    if pid is None:
                        try:
                            # Interrupted pg_ctl can precede postmaster.pid creation.
                            # This observation uses only Python's wide filesystem API.
                            pidfile = c["data"] / "postmaster.pid"
                            deadline = self.bounded_deadline(1)
                            while not c.get("stop_verified") and not pidfile.exists() and time.monotonic() < deadline:
                                time.sleep(0.1)
                            # Only the ASCII PID; no decoding or path comparison before stop.
                            pid = int(pidfile.read_bytes().splitlines()[0])
                            if pid <= 1:
                                raise RuntimeError("Invalid cleanup PID")
                        except Exception as e:
                            pid = None
                            rec["pid_read_error"] = str(e)
                    try:
                        cmd = self.run([self.bin(c, "pg_ctl"), "-D", c.get("data_arg", c["data"]),
                                        "-w", "-t", "15", "stop", "-m", "fast"], check=False, timeout=20)
                        rec["pg_ctl_stop"] = cmd
                        if cmd["returncode"]:
                            rec["clean_stop_error"] = cmd["stdout"] + cmd["stderr"]
                    except Exception as e:
                        rec["clean_stop_error"] = str(e)
                    if pid is None:
                        # An interrupted bootstrap may write its PID during the stop.
                        pidfile = c["data"] / "postmaster.pid"
                        if pidfile.exists():
                            pid = int(pidfile.read_bytes().splitlines()[0])
                        elif not c.get("stop_verified") and rec.get("pg_ctl_stop", {}).get("returncode") != 0:
                            if os.name == "nt":
                                # Stronger than command-line matching: no live postgres
                                # from this run's runtime can name ANY trial directory.
                                # This also covers relative -D arguments without a PEB read.
                                live = windows_private_processes(c["root"])
                                rec["no_pid_evidence"] = {"postmaster_pid_absent": True,
                                                          "live_private_postgres": list(live),
                                                          "scope": str(c["root"])}
                                if live:
                                    unresolved.append((c, rec))
                                    raise RuntimeError("Private postgres remains without a cluster PID; final owned-process sweep required")
                                rec["nothing_to_stop"] = True
                            else:
                                raise RuntimeError("Cannot verify interrupted start stopped without a PID or successful pg_ctl stop")
                    if pid is not None:
                        if pid <= 1:
                            raise RuntimeError("Invalid cleanup PID")
                        ident = self.cleanup_identity(pid, rec)
                        if ident is not None:
                            binary = os.path.normcase(str(self.bin(c, "postgres").resolve()))
                            private = os.path.normcase(ident[0]) == binary
                            if private and (c["identity"] is None or c["identity"] == ident):
                                c["pid"], c["identity"] = pid, ident
                                try:
                                    self.remember_descendants(c)
                                except Exception as e:
                                    rec["worker_enumeration_error"] = str(e)
                                self.hard_kill(c)  # Existing identified PID-only fallback.
                                deadline = self.bounded_deadline(10)
                                while self.cleanup_identity(pid, rec) == ident and time.monotonic() < deadline:
                                    time.sleep(0.1)
                                if self.cleanup_identity(pid, rec) == ident:
                                    raise RuntimeError("Cleanup hard kill did not end the postmaster")
                                rec["forced_stop"] = True
                            else:
                                rec["pid_reused"] = True
                    c["pid"], c["identity"] = None, None
                    c["stop_verified"] = True
                if not c["start_attempted"]:
                    rec["nothing_to_stop"] = True  # Failed/skipped initdb never launched a server.
                self.wait_descendants(c, rec, force=True)
                rec["stopped"] = True
                rec["status"] = "pass"
            except Exception as e:
                rec["error"] = str(e)
                failures.append(str(e))
            finally:
                self.command_cwd, self.env, self.command_deadline = old_cwd, old_env, old_deadline
                self.command_output_dir = old_output_dir
                rec["seconds"] = time.perf_counter() - before
            # Log capture is diagnostic only, and always happens after the stop attempt.
            def log(d):
                if c["log"].exists():
                    raw = self.decode(c["log"].read_bytes(), d, "postgres_log")
                    rec["postgres_log"] = self.redact(raw)
                    c["log"].write_text(self.redact(raw), encoding="utf-8")
            self.subcheck(rec, "log_capture", log)
            if rec.get("stopped") and c.get("junction"):
                try:
                    if c["junction"].exists():
                        windows_remove_junction(c["junction"])
                    c["junction_removed"] = rec["junction_removed_without_recursion"] = True
                except Exception as e:
                    rec["status"], rec["error"] = "fail", str(e)
                    failures.append(str(e))
        if final and os.name == "nt" and self.runtime is not None:
            sweep = details["owned_process_sweep"] = {"runtime": str(self.runtime), "terminated": []}
            try:
                if not contained(self.runtime, self.work):
                    raise RuntimeError("Runtime sweep path is outside this private run")
                # At the very end any private postgres from THIS runtime is owned,
                # including one left by an interrupted start without postmaster.pid.
                for pid, ident in windows_private_processes(self.runtime).items():
                    killed = windows_terminate_owned(pid, ident, self.runtime)
                    sweep["terminated"].append({"pid": pid, "identity": ident, "terminated": killed})
                remaining = windows_private_processes(self.runtime)
                sweep["remaining"] = list(remaining)
                if remaining:
                    raise RuntimeError("Private postgres processes remain after final sweep")
                sweep["status"] = "pass"
                for c, rec in unresolved:
                    if not (c["data"] / "postmaster.pid").exists():
                        failures.remove(rec.pop("error"))
                        rec.update(status="pass", stopped=True, nothing_to_stop=True,
                                   resolved_by_final_sweep=True)
                        c.update(pid=None, identity=None, stop_verified=True)
                        if c.get("junction") and c["junction"].exists():
                            windows_remove_junction(c["junction"])
                            c["junction_removed"] = rec["junction_removed_without_recursion"] = True
            except Exception as e:
                sweep.update(status="fail", error=str(e))
                failures.append(str(e))
        for link in list(self.result.get("junction_cleanup_pending", [])):
            try:
                windows_remove_junction(Path(link))
                self.result["junction_cleanup_pending"].remove(link)
            except Exception as e:
                failures.append(str(e))
        if failures:
            raise RuntimeError("\n".join(failures))

    def execute(self):
        interrupted = False
        c = None
        try:
            self.step(1, "Download, checksum, safe extraction", self.download)
            self.step(2, "Inventory and relocated runtime", self.inspect_inventory)
            self.step(3, "Native linkage and signatures", self.linkage)
            if "windows" in self.triple:
                matrix = self.result["windows_path_matrix"] = {"applicable": True}
                def experiment(d):
                    d["windows_path_matrix"] = matrix
                    self.windows_matrix(matrix)
                self.step(4, "Windows Unicode path lifecycle matrix (steps 4-9)", experiment)
                for n, title in [(5, "Start"), (6, "Search/authentication"), (7, "Stop/restart"),
                                 (8, "Crash recovery"), (9, "Dump/restore")]:
                    def matrix_result(d):
                        d.update({"matrix_step": 4, "outcome": matrix.get("outcome", "fail")})
                        if n == 6:
                            probe = getattr(self, "lc_ctype_probe", None)
                            if probe is None:
                                probe = {"status": "unavailable", "details": {"reason": "No running private server in Windows matrix"}}
                            d["lc_ctype_probe"] = probe
                            pdata = probe["details"]
                            if pdata.get("initdb", {}).get("status") == "pending":
                                remaining = self.started_monotonic + 960 - time.monotonic()
                                if remaining < 120:
                                    pdata["initdb"] = {"status": "skipped", "reason": "Windows matrix left less than 120 s", "remaining_seconds": remaining}
                                else:
                                    old_deadline = self.command_deadline
                                    try:
                                        self.command_deadline = self.bounded_deadline(remaining)
                                        self.subcheck(pdata, "initdb", lambda result: self.locale_initdb(
                                            {"root": self.runtime}, pdata["first_working_lc_ctype"], result))
                                    finally:
                                        self.command_deadline = old_deadline
                        if matrix.get("outcome") != "pass":
                            raise RuntimeError("Windows matrix acceptance failed; individual stages and skipped trials are recorded in step 4")
                    self.step(n, title + " — Windows matrix outcome", matrix_result)
            else:
                self.result["windows_path_matrix"] = {"applicable": False}
                def initialize(d):
                    nonlocal c
                    c = self.new_cluster(self.runtime, "unsigned")
                    self.init(c, d)
                self.step(4, "initdb: UTF-8, SCRAM, Unicode path", initialize)
                def cluster_step(fn):
                    def wrapped(d):
                        if c is None:
                            raise RuntimeError("Prerequisite unavailable: private cluster")
                        fn(c, d)
                    return wrapped
                for n, title, fn in [(5, "Start, socket, idle memory", self.start), (6, "Search and authentication", self.queries),
                                     (7, "Clean stop and persistence", self.cycle), (8, "Hard crash, recovery, second start", self.crash),
                                     (9, "Nightly dump and restore", self.backup)]:
                    self.step(n, title, cluster_step(fn))
            self.step(10, "macOS hardened runtime copy", self.signed)
        except KeyboardInterrupt:
            interrupted = True
            self.result["interrupted"] = True
        finally:
            # Keep a second Ctrl-C/SIGTERM from interrupting process cleanup.
            previous = {}
            for sig in (signal.SIGINT, signal.SIGTERM):
                previous[sig] = signal.signal(sig, signal.SIG_IGN)
            try:
                if "windows" in self.triple:
                    self.command_deadline = self.started_monotonic + 1040
                self.step(11, "Always stop private servers", self.cleanup)
                self.result["finished_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
                self.persist()
            finally:
                for sig, handler in previous.items():
                    signal.signal(sig, handler)
        print("\nPostgreSQL desktop spike — " + self.triple)
        for r in self.result["steps"]:
            error = r.get("error", "").splitlines()
            print(f"{r['step']:2} {r['status'].upper():4} {r['seconds']:8.3f}s {r['name']}" + (" — " + error[0][:130] if error else ""))
        for line in self.diagnostic_lines():
            print(line, flush=True)
        print(("Output had errors; inspect: " if self.result.get("output_errors") else "Full result: ") + str(self.json_out))
        if self.summary_md is not None:
            print("Markdown summary: " + str(self.summary_md))
        return 130 if interrupted else (1 if self.result.get("output_errors") or any(r["status"] == "fail" for r in self.result["steps"]) else 0)


def main(argv=None):
    if sys.version_info < (3, 12) or not hasattr(tarfile, "data_filter"):
        print(f"Python {platform.python_version()} found; run this script with python3.12 or newer (Python >=3.12 is required).", file=sys.stderr)
        return 2
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--work-dir", required=True, type=Path, help="Empty private directory. All outputs and data remain here.")
    parser.add_argument("--triple", choices=ASSETS, help="Defaults to native host. Foreign archives can be inventoried; execution fails clearly.")
    parser.add_argument("--json-out", type=Path, help="Path inside work-dir; relative paths resolve beneath work-dir (default result.json).")
    parser.add_argument("--summary-md", type=Path, help="New Markdown file inside work-dir; relative paths resolve beneath work-dir.")
    parser.add_argument("--sign-identity", help="macOS codesign identity name or hash for the optional team-signed hardened-runtime variant.")
    parser.add_argument("--force", action="store_true", help="Allow nonempty work-dir; create a fresh run subdirectory, never overwrite previous data.")
    args = parser.parse_args(argv)
    try:
        triple = args.triple or host_triple()
        # Resolve the user-selected root once; don't follow preexisting output links.
        root = args.work_dir.resolve()
        if root.exists() and not root.is_dir():
            raise ValueError("--work-dir must be a directory")
        if root.exists() and any(root.iterdir()) and not args.force:
            raise ValueError("--work-dir is not empty; choose an empty directory or use --force for a fresh nested run")
        def output_path(out, option):
            output = out if out.is_absolute() else root / out
            if not contained(output, root) or output.resolve() == root or output.exists() or output.is_symlink():
                raise ValueError(option + " must be a new file strictly inside --work-dir; existing files/escaping links are refused")
            rel = output.resolve().relative_to(root)
            reserved = {"empty.pgpass", "unpacked", "reference-tree-moved", "runtime", "nightly-test.dump"}
            first = rel.parts[0]
            if first in reserved or first.startswith(("command-", "postgresql-", "unsigned", "signed", "run-", "win-")):
                raise ValueError(option + " collides with an internal spike path; choose result.json/summary.md or reports/<name>")
            return output
        output = output_path(args.json_out or Path("result.json"), "--json-out")
        summary = output_path(args.summary_md, "--summary-md") if args.summary_md else None
        if summary and (contained(summary, output) or contained(output, summary)):
            raise ValueError("--json-out and --summary-md must be distinct files, neither containing the other")
        root.mkdir(parents=True, exist_ok=True)
        if args.force:
            work = root / ("run-" + secrets.token_hex(8))
            work.mkdir()
        else:
            work = root
        if args.force:
            if args.json_out is None:
                output = work / "result.json"
        spike = Spike(work, triple, output, output_root=root, summary_md=summary, sign_identity=args.sign_identity)
        def interrupt(signum, frame):
            raise KeyboardInterrupt
        signal.signal(signal.SIGTERM, interrupt)
        return spike.execute()
    except (OSError, ValueError) as e:
        parser.error(str(e))


if __name__ == "__main__":
    sys.exit(main())
