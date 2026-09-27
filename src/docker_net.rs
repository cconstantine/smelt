//! The address range a sandbox pod's Docker networks live in (SME-33), and
//! which addresses in it smelt treats as a container. Shared by the server
//! and the sandbox agent (`src/bin/sandbox_agent.rs` compiles this file in
//! with `#[path]`), so both judge an address the same way.

use std::net::Ipv4Addr;

/// Docker's default bridge in a sandbox pod (dockerd's `--bip`).
pub const DOCKER_BRIDGE_IP: &str = "172.20.0.1/16";
/// Where the pod's other Docker networks come from (dockerd's
/// `--default-address-pool`).
pub const DOCKER_NETWORK_POOL: &str = "172.21.0.0/16";
/// Both of the above: `172.20.0.0/14`. Clear of k3s's pod and service
/// ranges (`10.42/16`, `10.43/16`) and the homelab LAN (`192.168.x`),
/// which Docker's own default pools would overlap.
const POOL_BASE: Ipv4Addr = Ipv4Addr::new(172, 20, 0, 0);
const POOL_PREFIX: u32 = 14;

/// Whether `ip` is in the pod's Docker address range.
pub fn in_docker_pool(ip: Ipv4Addr) -> bool {
    let mask = u32::MAX << (32 - POOL_PREFIX);
    u32::from(ip) & mask == u32::from(POOL_BASE) & mask
}

/// The container address `host` names: an IPv4 literal in the pod's
/// Docker range, as the model reads it from `docker inspect`. Names don't
/// count; like on a Linux host, container names only resolve inside Docker
/// networks.
pub fn container_address(host: &str) -> Option<Ipv4Addr> {
    host.parse().ok().filter(|ip| in_docker_pool(*ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_docker_pool_is_exactly_172_20_0_0_slash_14() {
        for inside in ["172.20.0.1", "172.21.0.2", "172.23.255.255"] {
            assert!(in_docker_pool(inside.parse().unwrap()), "{inside}");
        }
        for outside in ["172.19.255.255", "172.24.0.1", "10.43.0.1", "192.168.8.130", "127.0.0.1"] {
            assert!(!in_docker_pool(outside.parse().unwrap()), "{outside}");
        }
    }

    #[test]
    fn test_container_address_is_only_an_ip_literal_in_the_pool() {
        assert_eq!(container_address("172.21.0.2"), Some(Ipv4Addr::new(172, 21, 0, 2)));
        for not in ["localhost", "web", "172.24.0.1", "10.43.0.1", "172.21.0.2.", "", "::1"] {
            assert_eq!(container_address(not), None, "{not:?}");
        }
    }

    #[test]
    fn test_the_dockerd_ranges_are_inside_the_pool() {
        let bridge: Ipv4Addr = DOCKER_BRIDGE_IP.split('/').next().unwrap().parse().unwrap();
        let networks: Ipv4Addr = DOCKER_NETWORK_POOL.split('/').next().unwrap().parse().unwrap();
        assert!(in_docker_pool(bridge));
        assert!(in_docker_pool(networks));
    }
}
