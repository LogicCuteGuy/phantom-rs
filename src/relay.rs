use std::collections::HashMap;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use rand::{RngExt, rng};
use tokio_nethernet::discovery::{
    DEFAULT_PORT as LAN_DISCOVERY_PORT, DiscoveryListener, DiscoveryListenerConfig,
    ServerData as NetherServerData, TransportLayer,
};
use tokio_nethernet::{Message as NetherMessage, NetherNetDialer, NetherNetDialerConfig, NetherNetStream};
use tokio_raknet::protocol::constants::DEFAULT_UNCONNECTED_MAGIC;
use tokio_raknet::protocol::packet::{RaknetPacket, UnconnectedPong};
use tokio_raknet::protocol::types::Advertisement;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use bytes::{Bytes, BytesMut};

use crate::ServerProtocol;

const DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(150);
const DISCOVERY_POLL_ATTEMPTS: usize = 8;

#[derive(Clone, Debug)]
pub struct RelayHostConfig {
    pub relay_url: String,
    pub room: String,
    pub target_server: SocketAddr,
    pub server_protocol: ServerProtocol,
}

#[derive(Clone, Debug)]
pub struct RelayPlayerConfig {
    pub relay_url: String,
    pub room: String,
    pub bind_port: u16,
}

pub async fn run_relay_host(config: RelayHostConfig) -> Result<(), Box<dyn Error>> {
    let server_id: u64 = rng().random();

    loop {
        let ws_url = relay_ws_url(&config.relay_url, "host", &config.room);

        info!("[Relay Host] Connecting to {}", ws_url);
        let (ws, _) = match connect_async(&ws_url).await {
            Ok(v) => v,
            Err(err) => {
                warn!("[Relay Host] Connect failed: {}. Retrying in 1s", err);
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };
        let (mut ws_writer, mut ws_reader) = ws.split();

        let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Vec<u8>>();

        tokio::spawn(async move {
            while let Some(payload) = out_rx.recv().await {
                if let Err(err) = ws_writer.send(Message::Binary(payload.into())).await {
                    error!("[Relay Host] Failed to send WS payload: {}", err);
                    break;
                }
            }
        });

        let mut proxy_sockets: HashMap<u32, Arc<UdpSocket>> = HashMap::new();
        let mut nethernet_senders: HashMap<u32, mpsc::UnboundedSender<Vec<u8>>> = HashMap::new();
        let mut cached_nethernet_data: Option<NetherServerData> = None;

        info!(
            "[Relay Host] Ready. Forwarding relay traffic to {}",
            config.target_server
        );
        if config.server_protocol == ServerProtocol::Nethernet {
            let discovery_target = SocketAddr::new(config.target_server.ip(), LAN_DISCOVERY_PORT);
            info!(
                "[Relay Host] NetherNet discovery target is {} (from --server IP)",
                discovery_target
            );
        }

        while let Some(msg) = ws_reader.next().await {
            let msg = match msg {
                Ok(m) => m,
                Err(err) => {
                    warn!("[Relay Host] Relay socket closed: {}", err);
                    break;
                }
            };

            if let Message::Binary(bin) = msg {
                if bin.len() < 4 {
                    continue;
                }

                let player_id = u32::from_be_bytes([bin[0], bin[1], bin[2], bin[3]]);
                let payload = &bin[4..];

                if payload.is_empty() {
                    proxy_sockets.remove(&player_id);
                    nethernet_senders.remove(&player_id);
                    continue;
                }

                if config.server_protocol == ServerProtocol::Nethernet
                    && matches!(payload.first().copied(), Some(0x01) | Some(0x02))
                {
                    let nether_data = query_nethernet_server_data(config.target_server).await
                        .or_else(|| cached_nethernet_data.clone())
                        .unwrap_or_else(default_nethernet_data);

                    cached_nethernet_data = Some(nether_data.clone());

                    if let Some(raknet_pong) =
                        build_raknet_pong_from_nethernet_ping(payload, &nether_data, server_id)
                    {
                        let mut framed = Vec::with_capacity(4 + raknet_pong.len());
                        framed.extend_from_slice(&player_id.to_be_bytes());
                        framed.extend_from_slice(&raknet_pong);

                        if out_tx.send(framed).is_err() {
                            break;
                        }

                        continue;
                    }
                }

                if config.server_protocol == ServerProtocol::Nethernet {
                    if let std::collections::hash_map::Entry::Vacant(entry) =
                        nethernet_senders.entry(player_id)
                    {
                        match connect_nethernet_stream(config.target_server).await {
                            Some(mut stream) => {
                                let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                                let relay_tx = out_tx.clone();

                                tokio::spawn(async move {
                                    loop {
                                        tokio::select! {
                                            maybe_payload = rx.recv() => {
                                                let Some(payload) = maybe_payload else {
                                                    break;
                                                };

                                                let reliable = is_raknet_reliable(&payload);
                                                debug!(
                                                    "[Relay Host] Sending to NetherNet stream: player={}, reliable={}, len={}, first=0x{:02X}",
                                                    player_id,
                                                    reliable,
                                                    payload.len(),
                                                    payload.first().copied().unwrap_or(0)
                                                );

                                                let nether_message = if reliable {
                                                    NetherMessage::reliable(Bytes::from(payload))
                                                } else {
                                                    NetherMessage::unreliable(Bytes::from(payload))
                                                };

                                                if stream.send(nether_message).await.is_err() {
                                                    break;
                                                }
                                            }
                                            incoming = stream.next() => {
                                                let incoming = match incoming {
                                                    Some(Ok(msg)) => msg.buffer,
                                                    Some(Err(_)) => break,
                                                    None => break,
                                                };

                                                debug!(
                                                    "[Relay Host] Received from NetherNet stream: player={}, len={}, first=0x{:02X}",
                                                    player_id,
                                                    incoming.len(),
                                                    incoming.first().copied().unwrap_or(0)
                                                );

                                                let Some(relay_payload) = normalize_raknet_payload(&incoming) else {
                                                    warn!(
                                                        "[Relay Host] Dropping non-RakNet NetherNet payload ({} bytes)",
                                                        incoming.len()
                                                    );
                                                    continue;
                                                };

                                                let mut framed = Vec::with_capacity(4 + relay_payload.len());
                                                framed.extend_from_slice(&player_id.to_be_bytes());
                                                framed.extend_from_slice(&relay_payload);

                                                if relay_tx.send(framed).is_err() {
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                });

                                entry.insert(tx);
                                info!(
                                    "[Relay Host] Opened NetherNet bridge stream for player {}",
                                    player_id
                                );
                            }
                            None => {
                                warn!(
                                    "[Relay Host] Failed to open NetherNet stream for player {}",
                                    player_id
                                );
                            }
                        }
                    }

                    let mut remove_player = false;
                    if let Some(tx) = nethernet_senders.get(&player_id) {
                        if tx.send(payload.to_vec()).is_err() {
                            remove_player = true;
                        }
                    }

                    if remove_player {
                        nethernet_senders.remove(&player_id);
                    }

                    continue;
                }

                if !proxy_sockets.contains_key(&player_id) {
                    let bind_addr = if config.target_server.is_ipv6() {
                        SocketAddr::new(IpAddr::V6("::".parse().expect("valid ipv6")), 0)
                    } else {
                        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
                    };

                    let socket = Arc::new(UdpSocket::bind(bind_addr).await?);
                    socket.connect(config.target_server).await?;

                    let recv_socket = Arc::clone(&socket);
                    let relay_tx = out_tx.clone();

                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 2048];

                        loop {
                            let read = match recv_socket.recv(&mut buf).await {
                                Ok(n) => n,
                                Err(_) => break,
                            };

                            let mut framed = Vec::with_capacity(4 + read);
                            framed.extend_from_slice(&player_id.to_be_bytes());
                            framed.extend_from_slice(&buf[..read]);

                            if relay_tx.send(framed).is_err() {
                                break;
                            }
                        }
                    });

                    proxy_sockets.insert(player_id, socket);
                }

                if let Some(sock) = proxy_sockets.get(&player_id) {
                    if let Err(err) = sock.send(payload).await {
                        warn!("[Relay Host] UDP send failed for player {}: {}", player_id, err);
                        proxy_sockets.remove(&player_id);
                    }
                }
            }
        }

        warn!("[Relay Host] Disconnected from relay. Reconnecting in 1s");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn connect_nethernet_stream(target_server: SocketAddr) -> Option<NetherNetStream> {
    let bind_addr = if target_server.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };

    let discovery = DiscoveryListener::bind(
        bind_addr,
        DiscoveryListenerConfig {
            broadcast_addr: SocketAddr::new(target_server.ip(), LAN_DISCOVERY_PORT),
            broadcast_interval: Duration::from_millis(250),
            ..Default::default()
        },
    )
    .await
    .ok()?;

    tokio::time::sleep(Duration::from_millis(450)).await;

    let responses = discovery.responses().await;
    let (remote_network_id, _) = responses.into_iter().next()?;

    let dialer = NetherNetDialer::connect_with_signaling(discovery, NetherNetDialerConfig::default());
    dialer.dial(remote_network_id.to_string()).await.ok()
}

async fn query_nethernet_server_data(target_server: SocketAddr) -> Option<NetherServerData> {
    let bind_addr = if target_server.is_ipv6() {
        "[::]:0"
    } else {
        "0.0.0.0:0"
    };

    let discovery = DiscoveryListener::bind(
        bind_addr,
        DiscoveryListenerConfig {
            broadcast_addr: SocketAddr::new(target_server.ip(), LAN_DISCOVERY_PORT),
            broadcast_interval: DISCOVERY_POLL_INTERVAL,
            ..Default::default()
        },
    )
    .await
    .ok()?;

    for _ in 0..DISCOVERY_POLL_ATTEMPTS {
        tokio::time::sleep(DISCOVERY_POLL_INTERVAL).await;
        let responses = discovery.responses().await;
        for (addr, payload) in &responses {
            log::debug!("[NetherNet Discovery] Got response from {}: {:02X?}", addr, payload);
            match hex::decode(payload) {
                Ok(raw) => match NetherServerData::decode(&raw) {
                    Some(data) => {
                        log::info!("[NetherNet Discovery] Decoded NetherServerData: {:?}", data);
                        return Some(data);
                    }
                    None => {
                        log::warn!("[NetherNet Discovery] Failed to decode NetherServerData from {} after hex decode", addr);
                    }
                },
                Err(err) => {
                    log::warn!("[NetherNet Discovery] Failed to hex decode payload from {}: {}", addr, err);
                }
            }
        }
    }
    None
}

fn build_raknet_pong_from_nethernet_ping(
    ping_payload: &[u8],
    nether: &NetherServerData,
    server_id: u64,
) -> Option<Vec<u8>> {
    let mut src = Bytes::copy_from_slice(ping_payload);
    let packet = RaknetPacket::decode(&mut src).ok()?;

    let ping_time = match packet {
        RaknetPacket::UnconnectedPing(p) => p.ping_time,
        RaknetPacket::UnconnectedPingOpenConnections(p) => p.ping_time,
        _ => return None,
    };

    let players = nether.player_count.max(0);
    let max_players = nether.max_player_count.max(players);

    let motd = format!(
        "MCPE;{};649;1.20.62;{};{};{};{};{};1;19132;19133;",
        fallback_string(&nether.server_name, "phantom relay"),
        players,
        max_players,
        server_id,
        fallback_string(&nether.level_name, "relay"),
        game_type_name(nether.game_type),
    );

    let pong = RaknetPacket::UnconnectedPong(UnconnectedPong {
        ping_time,
        server_guid: server_id,
        magic: DEFAULT_UNCONNECTED_MAGIC,
        advertisement: Advertisement(Some(Bytes::from(motd))),
    });

    let mut out = BytesMut::new();
    pong.encode(&mut out).ok()?;
    Some(out.to_vec())
}

fn normalize_raknet_payload(payload: &[u8]) -> Option<Vec<u8>> {
    let mut src = Bytes::copy_from_slice(payload);
    match RaknetPacket::decode(&mut src) {
        Ok(packet) => {
            let mut out = BytesMut::new();
            packet.encode(&mut out).ok()?;
            Some(out.to_vec())
        }
        Err(err) => {
            warn!(
                "[Relay Host] Unable to decode NetherNet payload as RakNet, passing through raw bytes: {} bytes ({})",
                payload.len(), err
            );
            Some(payload.to_vec())
        }
    }
}

fn is_raknet_reliable(payload: &[u8]) -> bool {
    if payload.is_empty() {
        return true;
    }
    payload[0] & 0xE0 != 0
}

fn default_nethernet_data() -> NetherServerData {
    NetherServerData {
        server_name: "phantom relay".to_string(),
        level_name: "relay".to_string(),
        game_type: 1,
        player_count: 0,
        max_player_count: 20,
        editor_world: false,
        hardcore: false,
        transport_layer: TransportLayer::NetherNet,
        connection_type: 4,
    }
}

fn game_type_name(value: u8) -> &'static str {
    match value {
        2 => "Creative",
        3 => "Adventure",
        _ => "Survival",
    }
}

fn fallback_string(value: &str, default_value: &str) -> String {
    if value.trim().is_empty() {
        return default_value.to_string();
    }

    value.to_string()
}

pub async fn run_relay_player(config: RelayPlayerConfig) -> Result<(), Box<dyn Error>> {
    let bind_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), config.bind_port);
    let server_socket = Arc::new(UdpSocket::bind(bind_addr).await?);
    server_socket.set_broadcast(true)?;

    info!("[Relay Player] Listening on {}", bind_addr);

    struct PlayerTunnel {
        tx: mpsc::UnboundedSender<Vec<u8>>,
        endpoints: Arc<tokio::sync::Mutex<Vec<SocketAddr>>>,
    }

    let mut clients: HashMap<u16, PlayerTunnel> = HashMap::new();
    let mut buf = vec![0u8; 2048];

    loop {
        let (read, client_addr) = server_socket.recv_from(&mut buf).await?;
        let payload = buf[..read].to_vec();
        let client_port = client_addr.port();

        if let std::collections::hash_map::Entry::Vacant(vacant) = clients.entry(client_port) {
            let ws_url = relay_ws_url(&config.relay_url, "player", &config.room);

            info!(
                "[Relay Player] New console client on port {} (addr {}). Opening relay tunnel...",
                client_port,
                client_addr
            );

            let (ws, _) = connect_async(&ws_url).await?;
            let (mut ws_writer, mut ws_reader) = ws.split();
            let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();

            let send_socket = Arc::clone(&server_socket);
            let endpoints = Arc::new(tokio::sync::Mutex::new(vec![client_addr]));
            let endpoints_task = Arc::clone(&endpoints);

            tokio::spawn(async move {
                while let Some(out) = rx.recv().await {
                    if ws_writer.send(Message::Binary(out.into())).await.is_err() {
                        break;
                    }
                }
            });

            tokio::spawn(async move {
                while let Some(msg) = ws_reader.next().await {
                    let msg = match msg {
                        Ok(m) => m,
                        Err(_) => break,
                    };

                    if let Message::Binary(bin) = msg {
                        let targets = endpoints_task.lock().await.clone();
                        for target in targets {
                            let _ = send_socket.send_to(&bin, target).await;
                        }
                    }
                }
            });

            vacant.insert(PlayerTunnel {
                tx,
                endpoints,
            });
        } else if let Some(tunnel) = clients.get(&client_port) {
            let mut endpoints = tunnel.endpoints.lock().await;
            if !endpoints.contains(&client_addr) {
                if endpoints.len() >= 8 {
                    endpoints.remove(0);
                }
                endpoints.push(client_addr);
                info!(
                    "[Relay Player] Added client endpoint for port {}: {}",
                    client_port,
                    client_addr
                );
            }
        }

        let mut remove_client = false;
        if let Some(tunnel) = clients.get(&client_port) {
            if tunnel.tx.send(payload).is_err() {
                remove_client = true;
            }
        }

        if remove_client {
            clients.remove(&client_port);
        }
    }
}

fn encode_room(room: &str) -> String {
    room.bytes()
        .flat_map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                vec![b as char]
            }
            _ => {
                let hi = b >> 4;
                let lo = b & 0x0f;
                vec!['%', hex(hi), hex(lo)]
            }
        })
        .collect()
}

fn relay_ws_url(base: &str, role: &str, room: &str) -> String {
    let mut out = base.to_string();

    if let Some(scheme_idx) = out.find("://") {
        let after_scheme = &out[scheme_idx + 3..];
        if !after_scheme.contains('/') {
            out.push('/');
        }
    }

    let sep = if out.contains('?') { '&' } else { '?' };
    out.push(sep);
    out.push_str("role=");
    out.push_str(role);
    out.push_str("&room=");
    out.push_str(&encode_room(room));

    out
}

fn hex(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'A' + (v - 10)) as char,
    }
}
