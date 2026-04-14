use std::io;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use log::{debug, info, trace, warn};
use rand::{RngExt, rng};
use socket2::{Domain, Protocol, Socket, Type};

use crate::client_map::ClientMap;
use crate::proto::{
    offline_pong, read_unconnected_ping, UNCONNECTED_PING_ID, UNCONNECTED_PONG_ID,
};

const MAX_MTU: usize = 1472;
const IDLE_CHECK_INTERVAL: Duration = Duration::from_secs(5);

pub struct ProxyServer {
    bind_address: SocketAddr,
    bound_port: u16,
    remote_server_address: SocketAddr,
    ping_server: Mutex<Option<Arc<UdpSocket>>>,
    ping_server_v6: Mutex<Option<Arc<UdpSocket>>>,
    server: Mutex<Option<Arc<UdpSocket>>>,
    client_map: Arc<ClientMap>,
    prefs: ProxyPrefs,
    dead: AtomicBool,
    server_offline: AtomicBool,
    server_id: i64,
}

#[derive(Clone, Debug)]
pub struct ProxyPrefs {
    pub bind_address: String,
    pub bind_port: u16,
    pub remote_server: String,
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
                        let mut ping6 = self
                            .ping_server_v6
                            .lock()
                            .expect("ping v6 lock poisoned");
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
            let mut ping6 = self
                .ping_server_v6
                .lock()
                .expect("ping v6 lock poisoned");
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
        info!("Listener starting up: {}", listener.local_addr().unwrap_or(self.bind_address));

        let mut packet_buffer = [0u8; MAX_MTU];

        while !self.dead.load(Ordering::SeqCst) {
            if let Err(err) = self.process_data_from_clients(&listener, &mut packet_buffer) {
                if err.kind() != io::ErrorKind::WouldBlock && err.kind() != io::ErrorKind::TimedOut {
                    warn!("Error while processing client data: {}", err);
                }
            }
        }

        info!("Listener shut down: {}", listener.local_addr().unwrap_or(self.bind_address));
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

        let listener_addr = listener.local_addr().unwrap_or(client);
        let this = Arc::clone(self);
        let on_new_connection = move |new_server_conn: Arc<UdpSocket>| {
            info!(
                "New connection from client {} -> {}",
                client,
                listener_addr
            );
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
