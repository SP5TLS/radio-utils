//! Shared multi-interface UDP discovery logic for Protocol 1 and Protocol 2.
//!
//! Both protocols follow the same pattern:
//! 1. Enumerate non-loopback IPv4 interfaces
//! 2. For each interface, bind a socket and send a discovery request to both
//!    the global broadcast and the interface-specific broadcast address
//! 3. Collect responses until the timeout expires, deduplicating by MAC
//!
//! The protocol-specific parts (request packet contents and response parsing)
//! are injected via closures.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::types::*;

/// Perform multi-interface UDP discovery.
///
/// `build_request` returns the raw discovery request packet.
/// `parse_response` attempts to parse a received datagram into a `DiscoveredDevice`.
/// `port` is the radio-side port to target (typically 1024).
/// `timeout` is how long to listen for responses.
pub(crate) async fn discover_on_interfaces(
    build_request: impl Fn() -> Vec<u8>,
    parse_response: impl Fn(&[u8], SocketAddr) -> Option<DiscoveredDevice> + Send + Sync + 'static,
    port: u16,
    timeout: Duration,
) -> Result<Vec<DiscoveredDevice>> {
    let (bind_addrs, targets) = {
        #[cfg(feature = "if-addrs")]
        {
            let mut ifaces_list: Vec<if_addrs::IfAddr> = Vec::new();
            if let Ok(ifaces) = if_addrs::get_if_addrs() {
                for iface in ifaces {
                    if !iface.is_loopback() {
                        if let if_addrs::IfAddr::V4(_) = iface.addr {
                            ifaces_list.push(iface.addr);
                        }
                    }
                }
            }

            if ifaces_list.is_empty() {
                (
                    vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))],
                    vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port))],
                )
            } else {
                let mut bind_addrs = Vec::new();
                let mut targets = Vec::new();
                for iface in ifaces_list {
                    if let if_addrs::IfAddr::V4(v4) = iface {
                        bind_addrs.push(SocketAddr::V4(SocketAddrV4::new(v4.ip, 0)));
                        targets.push(SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port)));
                        if let Some(bcast) = v4.broadcast {
                            bind_addrs.push(SocketAddr::V4(SocketAddrV4::new(v4.ip, 0)));
                            targets.push(SocketAddr::V4(SocketAddrV4::new(bcast, port)));
                        }
                    }
                }
                (bind_addrs, targets)
            }
        }

        #[cfg(not(feature = "if-addrs"))]
        {
            // Without if-addrs, fall back to default broadcast
            (
                vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))],
                vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::BROADCAST, port))],
            )
        }
    };

    discover_multi(build_request, parse_response, bind_addrs, targets, timeout).await
}

/// Perform discovery targeting a specific unicast address.
pub(crate) async fn discover_at_addr(
    build_request: impl Fn() -> Vec<u8>,
    parse_response: impl Fn(&[u8], SocketAddr) -> Option<DiscoveredDevice> + Send + Sync + 'static,
    addr: Ipv4Addr,
    port: u16,
    timeout: Duration,
) -> Result<Vec<DiscoveredDevice>> {
    discover_multi(
        build_request,
        parse_response,
        vec![SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0))],
        vec![SocketAddr::V4(SocketAddrV4::new(addr, port))],
        timeout,
    )
    .await
}

/// Core discovery implementation.
///
/// `bind_addrs` and `targets` are parallel vectors: socket `i` sends to target `i`.
/// Each (bind, target) pair is zipped — we do NOT send every socket to every target.
async fn discover_multi(
    build_request: impl Fn() -> Vec<u8>,
    parse_response: impl Fn(&[u8], SocketAddr) -> Option<DiscoveredDevice> + Send + Sync + 'static,
    bind_addrs: Vec<SocketAddr>,
    targets: Vec<SocketAddr>,
    timeout: Duration,
) -> Result<Vec<DiscoveredDevice>> {
    // Bind sockets and pair with their corresponding target.
    let mut socket_target_pairs = Vec::new();
    for (bind_addr, target) in bind_addrs.into_iter().zip(targets) {
        if let Ok(socket) = UdpSocket::bind(bind_addr).await {
            socket.set_broadcast(true).ok();
            socket_target_pairs.push((socket, target));
        }
    }

    if socket_target_pairs.is_empty() {
        return Err(crate::ProtocolError::Io(std::io::Error::other(
            "Failed to bind any discovery sockets",
        )));
    }

    let req = build_request();

    // Send from each socket to its paired target only.
    for (socket, target) in &socket_target_pairs {
        // Send multiple times to mitigate UDP drops (TAHOE fix)
        for _ in 0..2 {
            let _ = socket.send_to(&req, *target).await;
        }
    }

    // Spawn a receive task per socket, funneling results into a single mpsc channel.
    // This polls all sockets concurrently rather than sequentially.
    let (tx, mut rx) = mpsc::channel::<DiscoveredDevice>(64);
    let parse_response = std::sync::Arc::new(parse_response);
    let deadline = tokio::time::Instant::now() + timeout;

    let mut join_handles = Vec::new();
    for (socket, _target) in socket_target_pairs {
        let tx = tx.clone();
        let parse = parse_response.clone();
        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 128];
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await {
                    Ok(Ok((len, addr))) => {
                        if let Some(device) = parse(&buf[..len], addr) {
                            // If the receiver is dropped, stop.
                            if tx.send(device).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        log::warn!("Discovery recv error: {}", e);
                    }
                    Err(_) => {
                        // Timeout expired
                        break;
                    }
                }
            }
        });
        join_handles.push(handle);
    }

    // Drop our copy so the channel closes when all tasks finish.
    drop(tx);

    // Collect unique devices by MAC.
    let mut devices = Vec::new();
    let mut seen_macs = std::collections::HashSet::new();
    while let Some(device) = rx.recv().await {
        if seen_macs.insert(device.mac) {
            log::debug!("Discovered: {}", device);
            devices.push(device);
        }
    }

    // Wait for all tasks to finish (they should already be done since rx closed).
    for handle in join_handles {
        let _ = handle.await;
    }

    Ok(devices)
}
