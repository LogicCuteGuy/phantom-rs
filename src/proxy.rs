use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{debug, info, trace, warn};
use rand::{RngExt, rng};
use socket2::{Domain, Protocol, Socket, Type};
use tokio_nethernet::discovery::{
    DEFAULT_PORT as LAN_DISCOVERY_PORT, DiscoveryListener, DiscoveryListenerConfig,
    ServerData as NetherServerData, TransportLayer,
};

use crate::client_map::ClientMap;
use crate::proto::{
    offline_pong, read_unconnected_ping, PongData, UnconnectedPing, UNCONNECTED_PING_ID,
    UNCONNECTED_PONG_ID,
};

const MAX_MTU: usize = 1472;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const UNCONNECTED_PING_OPEN_CONNECTIONS_ID: u8 = 0x02;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerProtocol {
    Raknet,
    Nethernet,
}

pub struct ProxyServer {
    bind_address: SocketAddr,
    bound_port: u16,
    remote_server_address: SocketAddr,
    ping_server: Mutex<Option<Arc<UdpSocket>>>,
    ping_server_v6: Mutex<Option<Arc<UdpSocket>>>,
    server: Mutex<Option<Arc<UdpSocket>>>,
    client_map: Arc<ClientMap>,
    latest_pong: Mutex<Option<PongData>>,
    latest_nethernet_data: Mutex<Option<NetherServerData>>,
    prefs: ProxyPrefs,
    dead: AtomicBool,
    server_offline: AtomicBool,
    server_id: u64,
}

#[derive(Clone, Debug)]
pub struct ProxyPrefs {
    pub bind_address: String,
    pub bind_port: u16,
    pub remote_server: String,
    pub server_protocol: ServerProtocol,
    pub idle_timeout: Duration,
    pub enable_ipv6: bool,
    pub remove_ports: bool,
    pub num_workers: usize,
}

impl ProxyServer {
    pub fn new(mut prefs: ProxyPrefs) -> io::Result<Self> {
        let bind_port = if prefs.bind_port == 0 {
            rng().random_range(50_000..64_000)
        } else {
            prefs.bind_port
        };

        prefs.bind_port = bind_port;

        let bind_addr_str = format!("{}:{}", prefs.bind_address, bind_port);
        let bind_address = resolve_addr(&bind_addr_str)?;
        let remote_server_address = resolve_addr(&prefs.remote_server)?;

        Ok(Self {
            bind_address,
            bound_port: bind_port,
            remote_server_address,
            ping_server: Mutex::new(None),
            ping_server_v6: Mutex::new(None),
            server: Mutex::new(None),
            client_map: ClientMap::new(prefs.idle_timeout, IDLE_CHECK_INTERVAL),
            latest_pong: Mutex::new(None),
            latest_nethernet_data: Mutex::new(None),
            prefs,
            dead: AtomicBool::new(false),
            server_offline: AtomicBool::new(false),
            server_id: rng().random(),
        })
    }

    pub fn start(self: Arc<Self>) -> io::Result<()> {
        info!("Binding ping server to port 19132");
        let ping_server = Arc::new(bind_reusable_socket("0.0.0.0:19132", true, false)?);
        {
            let mut ping = self.ping_server.lock().expect("ping lock poisoned");
            *ping = Some(Arc::clone(&ping_server));
        }

        {
            let this = Arc::clone(&self);
            let listener = Arc::clone(&ping_server);
            thread::spawn(move || this.read_loop(listener));
        }

        if self.prefs.enable_ipv6 {
            info!("Binding IPv6 ping server to port 19133");
            match bind_reusable_socket("[::]:19133", true, true) {
                Ok(sock) => {
                    let sock = Arc::new(sock);
                    {
                        let mut ping6 = self.ping_server_v6.lock().expect("ping v6 lock poisoned");
                        *ping6 = Some(Arc::clone(&sock));
                    }

                    let this = Arc::clone(&self);
                    thread::spawn(move || this.read_loop(sock));
                }
                Err(err) => {
                    warn!("Failed to bind IPv6 ping listener: {}", err);
                }
            }
        }

        if self.prefs.server_protocol == ServerProtocol::Nethernet {
            Arc::clone(&self).start_nethernet_discovery_service();
        }

        info!("Binding proxy server to: {}", self.bind_address);
        let server = Arc::new(bind_reusable_socket(
            &self.bind_address.to_string(),
            true,
            self.bind_address.is_ipv6(),
        )?);

        {
            let mut server_slot = self.server.lock().expect("server lock poisoned");
            *server_slot = Some(Arc::clone(&server));
        }

        info!("Proxy server listening!");
        info!("Once your console pings phantom, you should see replies below.");
        self.start_workers(server);

        Ok(())
    }

    pub fn close(&self) {
        info!("Stopping proxy server");
        self.dead.store(true, Ordering::SeqCst);
        self.client_map.close();

        {
            let mut server = self.server.lock().expect("server lock poisoned");
            *server = None;
        }
        {
            let mut ping = self.ping_server.lock().expect("ping lock poisoned");
            *ping = None;
        }
        {
            let mut ping6 = self.ping_server_v6.lock().expect("ping v6 lock poisoned");
            *ping6 = None;
        }
    }

    fn start_workers(self: Arc<Self>, listener: Arc<UdpSocket>) {
        let workers = self.prefs.num_workers.max(1);
        info!("Starting {} workers", workers);

        for i in 0..workers {
            if i < workers - 1 {
                let this = Arc::clone(&self);
                let worker_listener = Arc::clone(&listener);
                thread::spawn(move || this.read_loop(worker_listener));
            } else {
                Arc::clone(&self).read_loop(Arc::clone(&listener));
            }
        }
    }

    fn read_loop(self: Arc<Self>, listener: Arc<UdpSocket>) {
        info!(
            "Listener starting up: {}",
            listener.local_addr().unwrap_or(self.bind_address)
        );

        let mut packet_buffer = [0u8; MAX_MTU];

        while !self.dead.load(Ordering::SeqCst) {
            if let Err(err) = self.process_data_from_clients(&listener, &mut packet_buffer) {
                if err.kind() != io::ErrorKind::WouldBlock && err.kind() != io::ErrorKind::TimedOut {
                    warn!("Error while processing client data: {}", err);
                }
            }
        }

        info!(
            "Listener shut down: {}",
            listener.local_addr().unwrap_or(self.bind_address)
        );
    }

    fn start_nethernet_discovery_service(self: Arc<Self>) {
        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    warn!("Failed to create Tokio runtime for NetherNet discovery: {}", err);
                    return;
                }
            };

            runtime.block_on(async move {
                let bind_addr = if self.bind_address.is_ipv6() {
                    format!("[::]:{}", LAN_DISCOVERY_PORT)
                } else {
                    format!("0.0.0.0:{}", LAN_DISCOVERY_PORT)
                };

                info!("Starting tokio-nethernet discovery listener on {}", bind_addr);
                let listener = match DiscoveryListener::bind(
                    &bind_addr,
                    DiscoveryListenerConfig {
                        broadcast_addr: SocketAddr::new(self.remote_server_address.ip(), LAN_DISCOVERY_PORT),
                        broadcast_interval: Duration::from_millis(500),
                        ..Default::default()
                    },
                )
                .await
                {
                    Ok(listener) => listener,
                    Err(err) => {
                        warn!("Failed to start tokio-nethernet discovery listener: {}", err);
                        return;
                    }
                };

                while !self.dead.load(Ordering::SeqCst) {
                    listener
                        .set_server_data(self.build_cached_nethernet_server_data())
                        .await;
                    tokio::time::sleep(Duration::from_millis(750)).await;
                }
            });
        });
    }

    fn process_data_from_clients(
        self: &Arc<Self>,
        listener: &Arc<UdpSocket>,
        packet_buffer: &mut [u8; MAX_MTU],
    ) -> io::Result<()> {
        let (read, client) = listener.recv_from(packet_buffer)?;
        if read == 0 {
            return Ok(());
        }

        let data = &packet_buffer[..read];
        trace!("client recv: {:?}", data);

        if (data[0] == UNCONNECTED_PING_ID || data[0] == UNCONNECTED_PING_OPEN_CONNECTIONS_ID)
            && self.prefs.server_protocol == ServerProtocol::Nethernet
        {
            if let Some(reply_bytes) = self.build_raknet_pong_from_nethernet(data) {
                if let Some(server_socket) = self.server_socket() {
                    let _ = server_socket.send_to(&reply_bytes, client);
                }
                info!("Sent NetherNet-backed LAN pong to client: {}", client);
                return Ok(());
            }

            let reply_buffer = offline_pong();
            let reply_bytes = self.rewrite_unconnected_pong(reply_buffer);
            if let Some(server_socket) = self.server_socket() {
                let _ = server_socket.send_to(&reply_bytes, client);
            }

            warn!(
                "Failed to fetch NetherNet MOTD; sent offline fallback pong to client: {}",
                client
            );
            return Ok(());
        }

        let listener_addr = listener.local_addr().unwrap_or(client);
        let this = Arc::clone(self);
        let on_new_connection = move |new_server_conn: Arc<UdpSocket>| {
            info!("New connection from client {} -> {}", client, listener_addr);
            let process_this = Arc::clone(&this);
            thread::spawn(move || process_this.process_data_from_server(new_server_conn, client));
        };

        let server_conn = self
            .client_map
            .get_or_create(client, self.remote_server_address, on_new_connection)?;

        server_conn.set_read_timeout(Some(Duration::from_secs(5)))?;

        if data[0] == UNCONNECTED_PING_ID {
            info!("Received LAN ping from client: {}", client);

            if self.server_offline.load(Ordering::SeqCst) {
                let reply_buffer = offline_pong();
                let reply_bytes = self.rewrite_unconnected_pong(reply_buffer);

                if let Some(server_socket) = self.server_socket() {
                    let _ = server_socket.send_to(&reply_bytes, client);
                }

                info!("Sent server offline pong to client: {}", client);
            }
        }

        let _ = server_conn.send(data)?;

        Ok(())
    }

    fn process_data_from_server(self: Arc<Self>, remote_conn: Arc<UdpSocket>, client: SocketAddr) {
        let mut buffer = [0u8; MAX_MTU];

        while !self.dead.load(Ordering::SeqCst) {
            match remote_conn.recv(&mut buffer) {
                Ok(read) => {
                    let _ = remote_conn.set_read_timeout(None);

                    if read == 0 {
                        continue;
                    }

                    if self.server_offline.load(Ordering::SeqCst) {
                        info!("Server is back online!");
                        self.server_offline.store(false, Ordering::SeqCst);
                    }

                    let mut data = buffer[..read].to_vec();
                    trace!("server recv: {:?}", data);

                    if data[0] == UNCONNECTED_PONG_ID {
                        data = self.rewrite_unconnected_pong(data);
                        info!("Sent LAN pong to client: {}", client);
                    }

                    if let Some(server_socket) = self.server_socket() {
                        let _ = server_socket.send_to(&data, client);
                    }
                }
                Err(err) => {
                    if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut {
                        if !self.server_offline.load(Ordering::SeqCst) {
                            warn!("Server seems to be offline :(");
                            warn!("We'll keep trying to connect...");
                            self.server_offline.store(true, Ordering::SeqCst);
                        }
                    } else {
                        warn!("{}", err);
                        let msg = err.to_string().to_lowercase();
                        if (msg.contains("timeout") || msg.contains("connection refused"))
                            && !self.server_offline.load(Ordering::SeqCst)
                        {
                            warn!("Server seems to be offline :(");
                            warn!("We'll keep trying to connect...");
                            self.server_offline.store(true, Ordering::SeqCst);
                        }
                    }
                    break;
                }
            }
        }

        self.client_map.delete(client);
    }

    fn build_cached_nethernet_server_data(&self) -> NetherServerData {
        if let Some(cached) = self
            .latest_nethernet_data
            .lock()
            .expect("latest nethernet data lock poisoned")
            .clone()
        {
            return cached;
        }

        let latest = self
            .latest_pong
            .lock()
            .expect("latest pong lock poisoned")
            .clone();

        if let Some(pong) = latest {
            let mut player_count = parse_i32_or_default(&pong.players, 0);
            let mut max_player_count = parse_i32_or_default(&pong.max_players, player_count.max(20));

            if player_count < 0 {
                player_count = 0;
            }
            if max_player_count < player_count {
                max_player_count = player_count;
            }

            return NetherServerData {
                server_name: fallback_string(&pong.motd, "phantom LAN Server"),
                level_name: fallback_string(&pong.sub_motd, "Bedrock level"),
                game_type: map_game_type(&pong.game_type),
                player_count,
                max_player_count,
                editor_world: false,
                hardcore: false,
                transport_layer: TransportLayer::RakNet,
                connection_type: 4,
            };
        }

        NetherServerData {
            server_name: "phantom LAN Server".to_string(),
            level_name: "Bedrock level".to_string(),
            game_type: 1,
            player_count: 0,
            max_player_count: 20,
            editor_world: false,
            hardcore: false,
            transport_layer: TransportLayer::RakNet,
            connection_type: 4,
        }
    }

    fn build_raknet_pong_from_nethernet(&self, ping_packet: &[u8]) -> Option<Vec<u8>> {
        if ping_packet.len() < 33
            || (ping_packet[0] != UNCONNECTED_PING_ID
                && ping_packet[0] != UNCONNECTED_PING_OPEN_CONNECTIONS_ID)
        {
            return None;
        }

        let server_data = self
            .fetch_nethernet_server_data()
            .or_else(|| {
                self.latest_nethernet_data
                    .lock()
                    .expect("latest nethernet data lock poisoned")
                    .clone()
            })?;

        let mut ping_time = [0u8; 8];
        ping_time.copy_from_slice(&ping_packet[1..9]);

        let mut id = [0u8; 8];
        id.copy_from_slice(&ping_packet[25..33]);

        let mut magic = [0u8; 16];
        magic.copy_from_slice(&ping_packet[9..25]);

        let mut port4 = self.bound_port.to_string();
        let mut port6 = port4.clone();

        if self.prefs.remove_ports {
            port4.clear();
            port6.clear();
        }

        let pong = PongData {
            edition: "MCPE".to_string(),
            motd: fallback_string(&server_data.server_name, "phantom LAN Server"),
            protocol_version: "649".to_string(),
            version: "1.20.62".to_string(),
            players: server_data.player_count.max(0).to_string(),
            max_players: server_data
                .max_player_count
                .max(server_data.player_count.max(0))
                .to_string(),
            server_id: self.server_id.to_string(),
            sub_motd: fallback_string(&server_data.level_name, "Bedrock level"),
            game_type: game_type_name(server_data.game_type).to_string(),
            nintendo_limited: "1".to_string(),
            port4,
            port6,
        };

        {
            let mut latest = self.latest_pong.lock().expect("latest pong lock poisoned");
            *latest = Some(pong.clone());
        }

        Some(
            UnconnectedPing {
                ping_time,
                id,
                magic,
                pong,
            }
            .build(),
        )
    }

    fn fetch_nethernet_server_data(&self) -> Option<NetherServerData> {
        if self.prefs.server_protocol != ServerProtocol::Nethernet {
            return None;
        }

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;

        let data = runtime.block_on(query_nethernet_server_data_async(self.remote_server_address))?;

        {
            let mut latest = self
                .latest_nethernet_data
                .lock()
                .expect("latest nethernet data lock poisoned");
            *latest = Some(data.clone());
        }

        Some(data)
    }

    fn rewrite_unconnected_pong(&self, data: Vec<u8>) -> Vec<u8> {
        debug!("Received Unconnected Pong from server: {:?}", data);

        match read_unconnected_ping(&data) {
            Ok(mut packet) => {
                packet.pong.server_id = self.server_id.to_string();

                if !packet.pong.port4.is_empty() && !self.prefs.remove_ports {
                    packet.pong.port4 = self.bound_port.to_string();
                    packet.pong.port6 = packet.pong.port4.clone();
                } else if self.prefs.remove_ports {
                    packet.pong.port4.clear();
                    packet.pong.port6.clear();
                }

                {
                    let mut latest = self.latest_pong.lock().expect("latest pong lock poisoned");
                    *latest = Some(packet.pong.clone());
                }

                let packet_buffer = packet.build();
                debug!("Unconnected Pong rewritten");
                packet_buffer
            }
            Err(err) => {
                warn!("Failed to rewrite pong: {}", err);
                data
            }
        }
    }

    fn server_socket(&self) -> Option<Arc<UdpSocket>> {
        self.server
            .lock()
            .expect("server lock poisoned")
            .as_ref()
            .map(Arc::clone)
    }
}

async fn query_nethernet_server_data_async(target_server: SocketAddr) -> Option<NetherServerData> {
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
    None
}

fn resolve_addr(addr: &str) -> io::Result<SocketAddr> {
    addr.to_socket_addrs()?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid address"))
}

fn bind_reusable_socket(addr: &str, with_timeout: bool, v6_only: bool) -> io::Result<UdpSocket> {
    let socket_addr = resolve_addr(addr)?;
    let domain = if socket_addr.is_ipv6() {
        Domain::IPV6
    } else {
        Domain::IPV4
    };

    let socket = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_reuse_address(true)?;

    if socket_addr.is_ipv6() {
        socket.set_only_v6(v6_only)?;
    }

    socket.bind(&socket_addr.into())?;

    let udp: UdpSocket = socket.into();
    if with_timeout {
        udp.set_read_timeout(Some(Duration::from_secs(1)))?;
    }

    Ok(udp)
}

fn parse_i32_or_default(input: &str, default: i32) -> i32 {
    input.parse::<i32>().unwrap_or(default)
}

fn fallback_string(value: &str, default: &str) -> String {
    if value.trim().is_empty() {
        return default.to_string();
    }

    value.to_string()
}

fn map_game_type(input: &str) -> u8 {
    if let Ok(raw) = input.parse::<u8>() {
        return raw;
    }

    let normalized = input.to_ascii_lowercase();

    if normalized.contains("creative") {
        return 2;
    }
    if normalized.contains("adventure") {
        return 3;
    }

    1
}

fn game_type_name(value: u8) -> &'static str {
    match value {
        2 => "Creative",
        3 => "Adventure",
        _ => "Survival",
    }
}
