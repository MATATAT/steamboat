use crate::SteamboatResult;
use anyhow::Context;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent, ServiceInfo};
use std::net::SocketAddr;
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

fn peer_from(info: &ResolvedService) -> Option<Peer> {
    let ip = info
        .addresses
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| info.addresses.iter().next())
        .map(|a| a.to_ip_addr())?;
    let name = info
        .fullname
        .split('.')
        .next()
        .unwrap_or(&info.fullname)
        .to_string();

    Some(Peer {
        name,
        addr: SocketAddr::new(ip, info.port),
    })
}

/// Browses for receivers for `window`, blocking. Results are deduplicated by
/// service fullname.
pub fn browse(window: Duration) -> SteamboatResult<Vec<Peer>> {
    let daemon = ServiceDaemon::new().context("starting mDNS daemon")?;
    let events = daemon
        .browse(SERVICE_TYPE)
        .context("browsing")?;
    let deadline = Instant::now() + window;
    let mut peers: Vec<(String, Peer)> = Vec::new();
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(remaining) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let fullname = info.fullname.clone();

                if !peers
                    .iter()
                    .any(|(existing, _)| *existing == fullname)
                {
                    peers.extend(peer_from(&info).map(|p| (fullname, p)));
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    daemon.shutdown().ok();

    Ok(peers
        .into_iter()
        .map(|(_, peer)| peer)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
