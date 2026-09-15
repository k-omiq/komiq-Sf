//! This server's addresses on the home network: what a phone on the same Wi-Fi types
//! to reach it. The console shows them when it is opened on the server itself, where
//! its own address is `localhost`.
//!
//! No interface-enumeration crate: asking the OS which local address it would use to
//! reach a few destinations (a UDP `connect` sends no packet) finds the address of
//! every interface that has a route. The desktop app does the same for the server it
//! runs (apps/admin/src-tauri/src/network.rs); keep the two in step.

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

fn routed_ipv4() -> Vec<Ipv4Addr> {
    PROBES.iter().filter_map(|p| source_for(p)).collect()
}

/// Inside a container the addresses and the port are the container's own, not what
/// another device uses (the host publishes its own port on its own address).
pub fn in_container() -> bool {
    ["/.dockerenv", "/run/.containerenv"]
        .iter()
        .any(|p| std::path::Path::new(p).exists())
}

/// A private, non-loopback socket peer on the home network. IPv4-mapped peers
/// are classified by their IPv4 address. Carrier-grade NAT is not a LAN signal.
pub fn is_lan_client(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
        }
    }
}

/// `http://<address>:<port>` for each way a device on the home network can reach the
/// listener bound to `bind`, the default route's first. Empty when it cannot be
/// known (a container) or there is none (a loopback-only listener).
pub fn addresses(bind: IpAddr, port: u16) -> Vec<String> {
    origins(bind, port, in_container(), routed_ipv4)
}

fn origins(
    bind: IpAddr,
    port: u16,
    in_container: bool,
    routed: impl Fn() -> Vec<Ipv4Addr>,
) -> Vec<String> {
    if in_container || bind.is_loopback() {
        return Vec::new();
    }
    let candidates = match bind {
        // Bound to one address: that is the only one that answers.
        IpAddr::V4(ip) if !ip.is_unspecified() => vec![ip],
        IpAddr::V6(ip) if !ip.is_unspecified() => return Vec::new(),
        _ => routed(),
    };
    let mut out: Vec<String> = Vec::new();
    for ip in candidates {
        let usable =
            !(ip.is_loopback() || ip.is_unspecified() || ip.is_link_local() || ip.is_broadcast());
        let origin = format!("http://{ip}:{port}");
        if usable && !out.contains(&origin) {
            out.push(origin);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANY: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    fn routed() -> Vec<Ipv4Addr> {
        vec![
            Ipv4Addr::new(192, 168, 1, 20),
            Ipv4Addr::new(192, 168, 1, 20),
            Ipv4Addr::new(169, 254, 3, 4),
            Ipv4Addr::LOCALHOST,
            Ipv4Addr::new(10, 0, 0, 7),
        ]
    }

    #[test]
    fn lan_client_ranges_exclude_loopback_and_public_peers() {
        for ip in [
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.2",
            "169.254.0.1",
            "fc00::1",
            "fdff::1",
            "fe80::1",
            "febf::1",
            "::ffff:192.168.1.2",
        ] {
            assert!(is_lan_client(ip.parse().unwrap()), "{ip}");
        }
        for ip in [
            "127.0.0.1",
            "::1",
            "0.0.0.0",
            "::",
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "169.253.1.1",
            "100.64.0.1",
            "2001:db8::1",
            "fec0::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!is_lan_client(ip.parse().unwrap()), "{ip}");
        }
    }

    #[test]
    fn every_interface_listener_lists_usable_addresses_once() {
        assert_eq!(
            origins(ANY, 8080, false, routed),
            ["http://192.168.1.20:8080", "http://10.0.0.7:8080"]
        );
    }

    #[test]
    fn a_listener_on_one_address_lists_only_that_address() {
        let bind = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5));
        assert_eq!(
            origins(bind, 7580, false, routed),
            ["http://192.168.1.5:7580"]
        );
    }

    #[test]
    fn loopback_and_containers_list_nothing() {
        let loopback = IpAddr::V4(Ipv4Addr::LOCALHOST);
        assert!(origins(loopback, 8080, false, routed).is_empty());
        assert!(origins(ANY, 8080, true, routed).is_empty());
    }

    #[test]
    fn the_real_probe_only_returns_usable_addresses() {
        for origin in addresses(ANY, 8080) {
            assert!(
                origin.starts_with("http://") && origin.ends_with(":8080"),
                "{origin}"
            );
            assert!(!origin.contains("127.0.0.1"), "{origin}");
        }
    }
}
