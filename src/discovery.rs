use crate::SteamboatResult;
use anyhow::Context;
use mdns_sd::{ResolvedService, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr, SocketAddrV6};
use std::time::{Duration, Instant};

pub const SERVICE_TYPE: &str = "_steamboat._tcp.local.";

#[derive(Debug, Clone)]
pub struct Peer {
    pub name: String,
    pub addr: SocketAddr,
}

pub struct Advertiser {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Advertiser {
    /// Registers `<instance>._steamboat._tcp.local.` on all interfaces.
    pub fn start(instance: &str, port: u16) -> SteamboatResult<Advertiser> {
        let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
        let host = format!("{instance}.local.");
        let service = ServiceInfo::new(SERVICE_TYPE, instance, &host, "", port, None)?.enable_addr_auto();
        let fullname = service
            .get_fullname()
            .to_string();
        daemon
            .register(service)
            .context("registering mDNS service")?;

        Ok(Advertiser { daemon, fullname })
    }
}

impl Drop for Advertiser {
    fn drop(&mut self) {
        self.daemon
            .unregister(&self.fullname)
            .ok();
        self.daemon.shutdown().ok();
    }
}

struct FoundService {
    fullname: String,
    port: u16,
    addresses: HashSet<ScopedIp>,
}

impl FoundService {
    fn absorb(&mut self, info: &ResolvedService) {
        self.addresses
            .extend(info.addresses.iter().cloned());
    }
}

impl From<&ResolvedService> for FoundService {
    fn from(info: &ResolvedService) -> FoundService {
        FoundService {
            fullname: info.fullname.clone(),
            port: info.port,
            addresses: info.addresses.clone(),
        }
    }
}

/// Lower is better: IPv4 routes everywhere, routable IPv6 usually does, and
/// link-local IPv6 only works with the right interface scope.
fn addr_rank(addr: &ScopedIp) -> u8 {
    match addr {
        ScopedIp::V4(_) => 0,
        ScopedIp::V6(v6)
            if !v6
                .addr()
                .is_unicast_link_local() =>
        {
            1
        }
        ScopedIp::V6(_) => 2,
        _ => u8::MAX,
    }
}

fn to_socket_addr(addr: &ScopedIp, port: u16) -> Option<SocketAddr> {
    match addr {
        ScopedIp::V4(v4) => Some(SocketAddr::new(IpAddr::V4(*v4.addr()), port)),
        ScopedIp::V6(v6) => {
            // A link-local address is unroutable without the scope of the
            // local interface it was discovered on.
            let scope = if v6
                .addr()
                .is_unicast_link_local()
            {
                v6.scope_id().index
            } else {
                0
            };

            Some(SocketAddr::V6(SocketAddrV6::new(*v6.addr(), port, 0, scope)))
        }
        _ => None,
    }
}

fn best_addr(addresses: &HashSet<ScopedIp>, port: u16) -> Option<SocketAddr> {
    addresses
        .iter()
        .min_by_key(|a| addr_rank(a))
        .and_then(|a| to_socket_addr(a, port))
}

fn instance_name(fullname: &str) -> String {
    fullname
        .split('.')
        .next()
        .unwrap_or(fullname)
        .to_string()
}

/// Browses for receivers for `window`, blocking. Repeated resolutions of the
/// same service (one per interface) are merged, and each peer gets its most
/// connectable address: IPv4, then routable IPv6, then scoped link-local IPv6.
pub fn browse(window: Duration) -> SteamboatResult<Vec<Peer>> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let events = daemon
        .browse(SERVICE_TYPE)
        .context("browsing")?;
    let deadline = Instant::now() + window;
    let mut found: Vec<FoundService> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                match found
                    .iter_mut()
                    .find(|f| f.fullname == info.fullname)
                {
                    Some(existing) => existing.absorb(&info),
                    None => found.push(FoundService::from(&*info)),
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    daemon.shutdown().ok();

    Ok(found
        .into_iter()
        .filter_map(|f| {
            best_addr(&f.addresses, f.port).map(|addr| Peer {
                name: instance_name(&f.fullname),
                addr,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn scoped(ip: &str) -> ScopedIp {
        ScopedIp::from(ip.parse::<IpAddr>().unwrap())
    }

    #[test]
    fn best_addr_prefers_ipv4_over_any_ipv6() {
        let addresses = HashSet::from([scoped("fe80::1"), scoped("2001:db8::1"), scoped("192.168.1.5")]);

        assert_eq!(
            best_addr(&addresses, 7),
            Some(
                "192.168.1.5:7"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn best_addr_prefers_routable_ipv6_over_link_local() {
        let addresses = HashSet::from([scoped("fe80::1"), scoped("2001:db8::1")]);

        assert_eq!(
            best_addr(&addresses, 7),
            Some(
                "[2001:db8::1]:7"
                    .parse()
                    .unwrap()
            )
        );
    }

    #[test]
    fn best_addr_falls_back_to_link_local() {
        let addresses = HashSet::from([scoped("fe80::1")]);
        let expected = SocketAddr::V6(SocketAddrV6::new("fe80::1".parse().unwrap(), 7, 0, 0));

        assert_eq!(best_addr(&addresses, 7), Some(expected));
    }

    #[test]
    fn best_addr_of_nothing_is_none() {
        assert_eq!(best_addr(&HashSet::new(), 7), None);
    }

    // Real multicast: run manually with `cargo test -- --ignored` on a LAN
    // without VPNs. Flaky on CI by nature, hence ignored.
    #[test]
    #[ignore]
    fn advertiser_is_discoverable_by_browse() {
        let _advertiser = Advertiser::start("steamboat-selftest", 45678).unwrap();
        let peers = browse(Duration::from_secs(5)).unwrap();

        assert!(
            peers
                .iter()
                .any(|p| p.name == "steamboat-selftest" && p.addr.port() == 45678)
        );
    }
}
