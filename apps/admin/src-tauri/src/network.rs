//! This computer's addresses on the home network, for the "Allow other devices" step.
//!
//! No interface-enumeration crate: asking the OS which local address it would use to
//! reach a few destinations (a UDP `connect` sends no packet) finds the address of every
//! interface that has a route, which is what another device on the same network can
//! reach. Loopback, unspecified and link-local answers are dropped.

use std::net::{IpAddr, Ipv4Addr, UdpSocket};

/// One public destination (the default route) and the usual private ranges (an
/// interface that only reaches the home network, e.g. Wi-Fi next to a VPN).
const PROBES: [&str; 4] = [
  "8.8.8.8:80",
  "192.168.0.1:80",
  "10.0.0.1:80",
  "172.16.0.1:80",
];

/// The OS's source address for traffic to `probe`, if it has a route there.
fn source_for(probe: &str) -> Option<Ipv4Addr> {
  let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
  socket.connect(probe).ok()?;
  match socket.local_addr().ok()?.ip() {
    IpAddr::V4(ip) => Some(ip),
    IpAddr::V6(_) => None,
  }
}

/// Whether a phone on the same network could plausibly use `ip`.
pub fn is_reachable_candidate(ip: Ipv4Addr) -> bool {
  !(ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() || ip.is_broadcast())
}

/// Distinct LAN IPv4 addresses, the default route's first.
pub fn lan_ipv4() -> Vec<Ipv4Addr> {
  let mut out: Vec<Ipv4Addr> = Vec::new();
  for probe in PROBES {
    if let Some(ip) = source_for(probe) {
      if is_reachable_candidate(ip) && !out.contains(&ip) {
        out.push(ip);
      }
    }
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn candidates_exclude_unusable_addresses() {
    assert!(is_reachable_candidate(Ipv4Addr::new(192, 168, 1, 20)));
    assert!(is_reachable_candidate(Ipv4Addr::new(10, 0, 0, 7)));
    assert!(!is_reachable_candidate(Ipv4Addr::LOCALHOST));
    assert!(!is_reachable_candidate(Ipv4Addr::UNSPECIFIED));
    assert!(!is_reachable_candidate(Ipv4Addr::new(169, 254, 3, 4)));
  }

  #[test]
  fn lan_addresses_are_distinct_and_usable() {
    let ips = lan_ipv4();
    for (i, ip) in ips.iter().enumerate() {
      assert!(is_reachable_candidate(*ip), "{ip}");
      assert!(!ips[i + 1..].contains(ip), "duplicate {ip}");
    }
  }
}
