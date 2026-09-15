//! Repair LAN access through the OS's own consent prompt. Builders stay portable so
//! tests can inspect every command without elevating or changing anyone's firewall.

use std::path::Path;

use serde::Serialize;

/// The UI offers a repair action only when this OS has a supported consent helper.
#[derive(Serialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Info {
  pub os: &'static str,
  pub automatic: bool,
}

pub fn info() -> Info {
  #[cfg(target_os = "windows")]
  return Info {
    os: "windows",
    automatic: true,
  };
  #[cfg(target_os = "linux")]
  return Info {
    os: "linux",
    automatic: on_path("pkexec") && (firewalld_running() || on_path("ufw")),
  };
  #[cfg(not(any(target_os = "windows", target_os = "linux")))]
  Info {
    os: "macos",
    automatic: false,
  }
}

/// Only letters, digits and plain path punctuation: PowerShell also ends a quoted
/// string at typographic quotes, so anything else gets a port-only rule instead.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn safe_windows_program(program: Option<&Path>) -> Option<&str> {
  program.and_then(Path::to_str).filter(|s| {
    !s.is_empty()
      && !s.ends_with('\\')
      && s
        .chars()
        .all(|c| c.is_alphanumeric() || " :\\/._-()".contains(c))
  })
}

/// Single-quoted PowerShell literals never expand variables or subexpressions.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn ps_literal(value: &str) -> String {
  format!("'{}'", value.replace('\'', "''"))
}

/// One elevated PowerShell (the UAC prompt) replaces the "komiq" rule and clears the
/// BLOCK rules Windows keeps for the server when its own prompt was cancelled; a block
/// wins over an allow. The firewall cmdlets take typed parameters, so no value passes
/// through netsh's command-line quoting. The rule covers every network profile but
/// only devices on the same network (`LocalSubnet`): Windows files most home Wi-Fi
/// under "Public", where a private-only rule would do nothing. The script travels as
/// -EncodedCommand so Start-Process cannot re-split it.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
fn windows_rule_args(port: u16, program: Option<&Path>) -> Vec<String> {
  let program = safe_windows_program(program);
  let scoped = program
    .map(|p| format!(" -Program {}", ps_literal(p)))
    .unwrap_or_default();
  let mut script = format!(
    "$ErrorActionPreference = 'Stop'; try {{ \
     Remove-NetFirewallRule -DisplayName 'komiq' -ErrorAction SilentlyContinue; \
     New-NetFirewallRule -DisplayName 'komiq' -Direction Inbound -Action Allow \
     -Protocol TCP -LocalPort {port} -RemoteAddress LocalSubnet{scoped} | Out-Null; "
  );
  if let Some(program) = program {
    // Enumerate local rules first: -Program is a wildcard query, whereas -eq is exact.
    // Never remove outgoing blocks or rules for any other executable.
    script.push_str(&format!(
      "Get-NetFirewallRule -PolicyStore PersistentStore | \
       Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' }} | \
       Get-NetFirewallApplicationFilter | Where-Object {{ $_.Program -eq {} }} | \
       Get-NetFirewallRule | Remove-NetFirewallRule; ",
      ps_literal(program)
    ));
  }
  script.push_str("exit 0 } catch { exit 2 }");
  let command = format!(
    "$encoded = [Convert]::ToBase64String([Text.Encoding]::Unicode.GetBytes({})); \
     try {{ $process = Start-Process -FilePath \"$PSHOME\\powershell.exe\" \
     -ArgumentList @('-NoProfile', '-NonInteractive', '-WindowStyle', 'Hidden', \
     '-EncodedCommand', $encoded) -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
     -ErrorAction Stop; exit $process.ExitCode }} catch {{ exit 3 }}",
    ps_literal(&script)
  );
  vec![
    "-NoProfile".into(),
    "-NonInteractive".into(),
    "-WindowStyle".into(),
    "Hidden".into(),
    "-Command".into(),
    command,
  ]
}

/// Runtime and permanent firewalld rules share one consent prompt; && preserves a
/// failure instead of reporting success from only the second command.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_firewalld_args(port: u16) -> Vec<String> {
  vec![
    "sh".into(),
    "-c".into(),
    format!("firewall-cmd --add-port={port}/tcp && firewall-cmd --permanent --add-port={port}/tcp"),
  ]
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn linux_ufw_args(port: u16) -> Vec<String> {
  vec!["ufw".into(), "allow".into(), format!("{port}/tcp")]
}

#[cfg(target_os = "linux")]
fn on_path(name: &str) -> bool {
  use std::os::unix::fs::PermissionsExt;
  std::env::var_os("PATH").is_some_and(|path| {
    std::env::split_paths(&path).any(|dir| {
      std::fs::metadata(dir.join(name))
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    })
  })
}

#[cfg(target_os = "linux")]
fn firewalld_running() -> bool {
  std::process::Command::new("firewall-cmd")
    .arg("--state")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

pub async fn open(port: u16) -> Result<String, String> {
  #[cfg(target_os = "windows")]
  {
    let program = crate::local::server_bin();
    let out = tokio::process::Command::new("powershell.exe")
      .args(windows_rule_args(port, Some(&program)))
      // CREATE_NO_WINDOW for the unelevated helper; WindowStyle hides the elevated one.
      .creation_flags(0x0800_0000)
      .output()
      .await
      .map_err(|e| format!("Could not request firewall permission: {e}"))?;
    return match out.status.code() {
      Some(0) => Ok(format!(
        "Windows now lets devices on your network reach komiq on port {port}."
      )),
      // Refused or cancelled at the Windows prompt: nothing was changed.
      Some(3) => Err("Windows did not give permission to change the firewall.".into()),
      _ => Err("Windows could not add the firewall rule.".into()),
    };
  }
  #[cfg(target_os = "linux")]
  {
    if !on_path("pkexec") {
      return Err("Install pkexec to let komiq request firewall permission.".into());
    }
    let firewalld = firewalld_running();
    let args = if firewalld {
      linux_firewalld_args(port)
    } else if on_path("ufw") {
      linux_ufw_args(port)
    } else {
      return Err("No running firewalld or installed ufw was found.".into());
    };
    let out = tokio::process::Command::new("pkexec")
      .args(args)
      .output()
      .await
      .map_err(|e| format!("Could not request firewall permission: {e}"))?;
    if out.status.success() {
      return Ok(format!(
        "Allowed TCP port {port} through {}.",
        if firewalld {
          "firewalld for now and future starts"
        } else {
          "ufw"
        }
      ));
    }
    return Err(match out.status.code() {
      Some(126) => "The firewall permission prompt was cancelled.".into(),
      Some(127) => "Permission to change the firewall was not granted.".into(),
      _ => {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let reason = stderr
          .lines()
          .find(|line| !line.trim().is_empty())
          .unwrap_or("the firewall command failed")
          .trim();
        format!("The firewall rule could not be added: {reason}")
      }
    });
  }
  #[cfg(not(any(target_os = "windows", target_os = "linux")))]
  {
    let _ = port;
    Err("macOS asks by itself. Choose Allow when it does. To change it later: System Settings › Network › Firewall › Options.".into())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The script the elevated PowerShell runs, taken back out of the outer command.
  fn inner_script(args: &[String]) -> String {
    assert_eq!(
      &args[..5],
      ["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command"]
    );
    let command = &args[5];
    let start = command.find("GetBytes('").unwrap() + "GetBytes('".len();
    let end = command.find("')); try").unwrap();
    assert!(command[end..].contains("-Verb RunAs -Wait"));
    assert!(command.ends_with("catch { exit 3 }"));
    command[start..end].replace("''", "'")
  }

  #[test]
  fn windows_scoped_rule_args() {
    let args = windows_rule_args(
      7580,
      Some(Path::new(r"C:\Program Files\komiq\komika-server.exe")),
    );
    assert_eq!(
      inner_script(&args),
      r"$ErrorActionPreference = 'Stop'; try { Remove-NetFirewallRule -DisplayName 'komiq' -ErrorAction SilentlyContinue; New-NetFirewallRule -DisplayName 'komiq' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 7580 -RemoteAddress LocalSubnet -Program 'C:\Program Files\komiq\komika-server.exe' | Out-Null; Get-NetFirewallRule -PolicyStore PersistentStore | Where-Object { $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' } | Get-NetFirewallApplicationFilter | Where-Object { $_.Program -eq 'C:\Program Files\komiq\komika-server.exe' } | Get-NetFirewallRule | Remove-NetFirewallRule; exit 0 } catch { exit 2 }"
    );
  }

  #[test]
  fn windows_port_only_rule_args() {
    assert_eq!(
      inner_script(&windows_rule_args(65535, None)),
      "$ErrorActionPreference = 'Stop'; try { Remove-NetFirewallRule -DisplayName 'komiq' -ErrorAction SilentlyContinue; New-NetFirewallRule -DisplayName 'komiq' -Direction Inbound -Action Allow -Protocol TCP -LocalPort 65535 -RemoteAddress LocalSubnet | Out-Null; exit 0 } catch { exit 2 }"
    );
  }

  #[test]
  fn windows_keeps_a_path_with_letters_of_any_alphabet() {
    let args = windows_rule_args(7580, Some(Path::new("C:\\Users\\José\\komika-server.exe")));
    assert!(inner_script(&args).contains("-Program 'C:\\Users\\José\\komika-server.exe'"));
  }

  #[test]
  fn windows_unsafe_paths_drop_program_and_block_cleanup() {
    for path in [
      r#"C:\bad"path\server.exe"#,
      "C:\\it's\\server.exe",
      "C:\\it\u{2019}s\\server.exe",
      "C:\\$bad\\server.exe",
      "C:\\bad`path\\server.exe",
      "C:\\bad;path\\server.exe",
      "C:\\bad\npath\\server.exe",
      "C:\\bad&path\\server.exe",
      "C:\\bad%path\\server.exe",
      "C:\\bad\\",
      "",
    ] {
      assert_eq!(
        windows_rule_args(7580, Some(Path::new(path))),
        windows_rule_args(7580, None),
        "{path:?}"
      );
    }
  }

  #[test]
  fn linux_firewalld_rule_args() {
    assert_eq!(
      linux_firewalld_args(7580),
      vec![
        "sh",
        "-c",
        "firewall-cmd --add-port=7580/tcp && firewall-cmd --permanent --add-port=7580/tcp"
      ]
    );
  }

  #[test]
  fn linux_ufw_rule_args() {
    assert_eq!(linux_ufw_args(7580), vec!["ufw", "allow", "7580/tcp"]);
  }

  #[test]
  fn linux_only_port_varies() {
    for port in [0, 1, 80, 7580, u16::MAX] {
      assert_eq!(
        linux_firewalld_args(port),
        vec![
          "sh".to_string(),
          "-c".to_string(),
          "firewall-cmd --add-port=PORT/tcp && firewall-cmd --permanent --add-port=PORT/tcp"
            .replace("PORT", &port.to_string())
        ]
      );
      assert_eq!(
        linux_ufw_args(port),
        vec![
          "ufw".to_string(),
          "allow".to_string(),
          "PORT/tcp".replace("PORT", &port.to_string())
        ]
      );
    }
  }

  #[test]
  fn firewall_info_json_shape() {
    for os in ["macos", "windows", "linux"] {
      assert_eq!(
        serde_json::to_value(Info {
          os,
          automatic: os != "macos"
        })
        .unwrap(),
        serde_json::json!({ "os": os, "automatic": os != "macos" })
      );
    }
  }
}
