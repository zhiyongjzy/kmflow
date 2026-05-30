use anyhow::Result;
use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, info, warn};

const DISCOVERY_PORT: u16 = 4243;
const ANNOUNCE_INTERVAL: Duration = Duration::from_secs(3);
const PEER_TTL: Duration = Duration::from_secs(10); // 3× announce + margin
const MAGIC: &[u8; 6] = b"KMFLOW";

#[derive(Debug, Clone)]
pub struct DiscoveredPeer {
    pub hostname: String,
    pub addr: SocketAddr,
    pub fingerprint: String,
    pub last_seen: Instant,
}

pub struct Discovery {
    hostname: String,
    port: u16,
    fingerprint: String,
    peers: Arc<RwLock<HashMap<String, DiscoveredPeer>>>,
    lan_info: Option<LanInfo>,
}

#[derive(Clone)]
struct LanInfo {
    local_addr: Ipv4Addr,
    broadcast_addr: Ipv4Addr,
}

impl Discovery {
    pub fn new(hostname: &str, port: u16, fingerprint: &str) -> Result<Self> {
        let lan_info = find_lan_info();
        if let Some(ref info) = lan_info {
            info!(iface_addr = %info.local_addr, broadcast = %info.broadcast_addr, "UDP broadcast discovery ready");
        } else {
            warn!("could not determine LAN broadcast address, discovery disabled");
        }

        Ok(Self {
            hostname: hostname.to_string(),
            port,
            fingerprint: fingerprint.to_string(),
            peers: Arc::new(RwLock::new(HashMap::new())),
            lan_info,
        })
    }

    pub fn browse(&self) -> Result<()> {
        self.browse_with_notify(None)
    }

    pub fn browse_with_notify(&self, notify_tx: Option<mpsc::Sender<SocketAddr>>) -> Result<()> {
        let Some(lan_info) = self.lan_info.clone() else {
            warn!("no LAN interface found, skipping discovery");
            return Ok(());
        };

        let hostname = self.hostname.clone();
        let port = self.port;
        let fingerprint = self.fingerprint.clone();
        let peers = self.peers.clone();

        // Announce packet format: MAGIC(6) + port(2 big-endian) + fingerprint_len(1) + fingerprint + hostname
        let announce_packet = build_announce(&hostname, port, &fingerprint);

        // Spawn sender: periodically broadcast announce
        let broadcast_addr = lan_info.broadcast_addr;
        let packet_clone = announce_packet.clone();
        tokio::spawn(async move {
            let sock = match UdpSocket::bind(SocketAddrV4::new(lan_info.local_addr, 0)) {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "failed to bind announce socket");
                    return;
                }
            };
            let _ = sock.set_broadcast(true);

            loop {
                let dest = SocketAddrV4::new(broadcast_addr, DISCOVERY_PORT);
                if let Err(e) = sock.send_to(&packet_clone, dest) {
                    debug!(?e, "broadcast send failed");
                }
                tokio::time::sleep(ANNOUNCE_INTERVAL).await;
            }
        });

        // Spawn receiver: listen for announce packets from peers
        let my_hostname = hostname.clone();
        tokio::spawn(async move {
            let sock =
                match UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT)) {
                    Ok(s) => {
                        let _ = s.set_nonblocking(true);
                        s
                    }
                    Err(e) => {
                        warn!(
                            ?e,
                            "failed to bind discovery listener on port {}", DISCOVERY_PORT
                        );
                        return;
                    }
                };

            let tokio_sock = match tokio::net::UdpSocket::from_std(sock) {
                Ok(s) => s,
                Err(e) => {
                    warn!(?e, "failed to convert to tokio socket");
                    return;
                }
            };

            let mut buf = [0u8; 512];
            loop {
                match tokio_sock.recv_from(&mut buf).await {
                    Ok((len, src_addr)) => {
                        if let Some((peer_hostname, peer_port, peer_fp)) =
                            parse_announce(&buf[..len])
                        {
                            if peer_hostname == my_hostname {
                                continue;
                            }
                            let peer_addr = SocketAddr::new(src_addr.ip(), peer_port);
                            let key = format!("{}:{}", peer_hostname, peer_port);

                            let already_known = peers.read().await.contains_key(&key);
                            if !already_known {
                                let peer = DiscoveredPeer {
                                    hostname: peer_hostname.clone(),
                                    addr: peer_addr,
                                    fingerprint: peer_fp,
                                    last_seen: Instant::now(),
                                };
                                info!(hostname = %peer.hostname, addr = %peer.addr, "discovered peer via broadcast");
                                peers.write().await.insert(key, peer);
                                if let Some(ref tx) = notify_tx {
                                    let _ = tx.send(peer_addr).await;
                                }
                            } else {
                                // Update last_seen for existing peer
                                if let Some(p) = peers.write().await.get_mut(&key) {
                                    p.last_seen = Instant::now();
                                    p.addr = peer_addr;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(?e, "discovery recv error");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });

        // Spawn cleanup task: expire stale peers
        let cleanup_peers = self.peers.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PEER_TTL).await;
                let mut peers = cleanup_peers.write().await;
                let before = peers.len();
                peers.retain(|_, p| p.last_seen.elapsed() < PEER_TTL);
                let removed = before - peers.len();
                if removed > 0 {
                    debug!(removed, remaining = peers.len(), "expired stale peers");
                }
            }
        });

        Ok(())
    }

    pub async fn get_peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read().await.values().cloned().collect()
    }

    pub fn shutdown(self) -> Result<()> {
        Ok(())
    }
}

fn build_announce(hostname: &str, port: u16, fingerprint: &str) -> Vec<u8> {
    let fp_bytes = fingerprint.as_bytes();
    let host_bytes = hostname.as_bytes();
    let mut pkt = Vec::with_capacity(6 + 2 + 1 + fp_bytes.len() + host_bytes.len());
    pkt.extend_from_slice(MAGIC);
    pkt.extend_from_slice(&port.to_be_bytes());
    pkt.push(fp_bytes.len() as u8);
    pkt.extend_from_slice(fp_bytes);
    pkt.extend_from_slice(host_bytes);
    pkt
}

fn parse_announce(data: &[u8]) -> Option<(String, u16, String)> {
    if data.len() < 9 || &data[..6] != MAGIC {
        return None;
    }
    let port = u16::from_be_bytes([data[6], data[7]]);
    let fp_len = data[8] as usize;
    if data.len() < 9 + fp_len {
        return None;
    }
    let fingerprint = String::from_utf8_lossy(&data[9..9 + fp_len]).to_string();
    let hostname = String::from_utf8_lossy(&data[9 + fp_len..]).to_string();
    if hostname.is_empty() {
        return None;
    }
    Some((hostname, port, fingerprint))
}

/// Find LAN interface info: local IP and broadcast address.
fn find_lan_info() -> Option<LanInfo> {
    let output = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show"])
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout);

    for line in stdout.lines() {
        // Format: "2: eth0    inet 192.168.66.99/24 brd 192.168.66.255 ..."
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 6 {
            continue;
        }
        let iface = parts[1].trim_end_matches(':');
        let addr_cidr = parts[3];
        let addr_str = addr_cidr.split('/').next().unwrap_or("");

        // Skip non-physical interfaces
        if iface == "lo"
            || iface.starts_with("docker")
            || iface.starts_with("br-")
            || iface.starts_with("veth")
            || iface.starts_with("virbr")
        {
            continue;
        }

        // Check if it's a private IP
        if addr_str.starts_with("192.168.")
            || addr_str.starts_with("10.")
            || is_172_private(addr_str)
        {
            // Find broadcast address in the line
            let brd_addr = parts
                .iter()
                .zip(parts.iter().skip(1))
                .find(|(k, _)| **k == "brd")
                .map(|(_, v)| *v);

            if let (Ok(local_addr), Some(brd_str)) = (addr_str.parse::<Ipv4Addr>(), brd_addr) {
                if let Ok(broadcast_addr) = brd_str.parse::<Ipv4Addr>() {
                    return Some(LanInfo {
                        local_addr,
                        broadcast_addr,
                    });
                }
            }
        }
    }
    None
}

fn is_172_private(addr: &str) -> bool {
    if let Some(rest) = addr.strip_prefix("172.") {
        if let Some(second) = rest.split('.').next() {
            if let Ok(n) = second.parse::<u8>() {
                return (16..=31).contains(&n);
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn announce_roundtrip() {
        let pkt = build_announce("my-host", 4242, "sha256:abc123");
        let parsed = parse_announce(&pkt).unwrap();
        assert_eq!(parsed.0, "my-host");
        assert_eq!(parsed.1, 4242);
        assert_eq!(parsed.2, "sha256:abc123");
    }

    #[test]
    fn announce_roundtrip_unicode_hostname() {
        let pkt = build_announce("我的电脑", 9999, "fp:xyz");
        let parsed = parse_announce(&pkt).unwrap();
        assert_eq!(parsed.0, "我的电脑");
        assert_eq!(parsed.1, 9999);
    }

    #[test]
    fn parse_announce_too_short() {
        assert!(parse_announce(b"short").is_none());
    }

    #[test]
    fn parse_announce_bad_magic() {
        let mut pkt = build_announce("host", 4242, "fp");
        pkt[0] = b'X'; // corrupt magic
        assert!(parse_announce(&pkt).is_none());
    }

    #[test]
    fn parse_announce_empty_hostname() {
        // Build a packet with empty hostname
        let mut pkt = Vec::new();
        pkt.extend_from_slice(MAGIC);
        pkt.extend_from_slice(&4242u16.to_be_bytes());
        pkt.push(2); // fingerprint len
        pkt.extend_from_slice(b"fp");
        // no hostname bytes
        assert!(parse_announce(&pkt).is_none());
    }

    #[test]
    fn is_172_private_range() {
        assert!(is_172_private("172.16.0.1"));
        assert!(is_172_private("172.31.255.255"));
        assert!(!is_172_private("172.15.0.1"));
        assert!(!is_172_private("172.32.0.1"));
        assert!(!is_172_private("10.0.0.1"));
    }
}
