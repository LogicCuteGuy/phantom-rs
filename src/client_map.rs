use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::info;

#[derive(Clone)]
struct ClientEntry {
    conn: Arc<UdpSocket>,
    last_active: Instant,
}

pub struct ClientMap {
    idle_timeout: Duration,
    idle_check_interval: Duration,
    clients: Mutex<HashMap<SocketAddr, ClientEntry>>,
    dead: AtomicBool,
}

impl ClientMap {
    pub fn new(idle_timeout: Duration, idle_check_interval: Duration) -> Arc<Self> {
        let map = Arc::new(Self {
            idle_timeout,
            idle_check_interval,
            clients: Mutex::new(HashMap::new()),
            dead: AtomicBool::new(false),
        });

        let cleanup_map = Arc::clone(&map);
        thread::spawn(move || cleanup_map.idle_cleanup_loop());

        map
    }

    pub fn close(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }

        let mut clients = self.clients.lock().expect("client map lock poisoned");
        clients.clear();
    }

    pub fn delete(&self, client_addr: SocketAddr) {
        let mut clients = self.clients.lock().expect("client map lock poisoned");
        clients.remove(&client_addr);
    }

    pub fn get_or_create<F>(
        &self,
        client_addr: SocketAddr,
        remote: SocketAddr,
        on_new_connection: F,
    ) -> io::Result<Arc<UdpSocket>>
    where
        F: FnOnce(Arc<UdpSocket>) + Send + 'static,
    {
        {
            let mut clients = self.clients.lock().expect("client map lock poisoned");
            if let Some(existing) = clients.get_mut(&client_addr) {
                existing.last_active = Instant::now();
                return Ok(Arc::clone(&existing.conn));
            }
        }

        info!(
            "Opening connection to {} for new client {}",
            remote, client_addr
        );

        let conn = Arc::new(new_server_connection(remote)?);
        {
            let mut clients = self.clients.lock().expect("client map lock poisoned");
            clients.insert(
                client_addr,
                ClientEntry {
                    conn: Arc::clone(&conn),
                    last_active: Instant::now(),
                },
            );
        }

        on_new_connection(Arc::clone(&conn));

        Ok(conn)
    }

    fn idle_cleanup_loop(&self) {
        info!("Starting idle connection handler");

        while !self.dead.load(Ordering::SeqCst) {
            thread::sleep(self.idle_check_interval);

            if self.dead.load(Ordering::SeqCst) {
                break;
            }

            let now = Instant::now();
            let mut clients = self.clients.lock().expect("client map lock poisoned");

            clients.retain(|client_addr, entry| {
                let is_idle = entry.last_active + self.idle_timeout <= now;
                if is_idle {
                    info!("Cleaning up idle connection: {}", client_addr);
                }
                !is_idle
            });
        }
    }
}

fn new_server_connection(remote: SocketAddr) -> io::Result<UdpSocket> {
    info!("Opening connection to {}", remote);

    let bind_addr: SocketAddr = if remote.is_ipv6() {
        "[::]:0".parse().expect("valid ipv6 wildcard")
    } else {
        "0.0.0.0:0".parse().expect("valid ipv4 wildcard")
    };

    let conn = UdpSocket::bind(bind_addr)?;
    conn.connect(remote)?;

    Ok(conn)
}
