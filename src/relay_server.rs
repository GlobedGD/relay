use std::{collections::HashMap, net::SocketAddr, sync::Arc, time::Duration};

use crate::logger::{debug, error, info, warn};
use parking_lot::Mutex as SyncMutex;
use tokio::{
    net::{TcpListener, UdpSocket},
    sync::Notify,
};

use crate::relay_client::{RELAY_MAGIC, RelayClient, RelayError};

pub struct RelayServer {
    servers: Vec<SocketAddr>,
    tcp_socket: TcpListener,
    udp_socket: Arc<UdpSocket>,
    clients: SyncMutex<HashMap<SocketAddr, Arc<RelayClient>>>,
    udp_awaiters: SyncMutex<HashMap<u32, Arc<Notify>>>,
    udp_accepted_queue: SyncMutex<Vec<(u32, SocketAddr)>>,
}

impl RelayServer {
    pub async fn new(host_addr: SocketAddr, servers: Vec<SocketAddr>) -> Result<Self, Box<dyn std::error::Error>> {
        let tcp_socket = TcpListener::bind(host_addr).await?;
        let udp_socket = UdpSocket::bind(host_addr).await?;

        Ok(Self {
            servers,
            tcp_socket,
            udp_socket: Arc::new(udp_socket),
            clients: SyncMutex::new(HashMap::default()),
            udp_awaiters: SyncMutex::new(HashMap::default()),
            udp_accepted_queue: SyncMutex::new(Vec::new()),
        })
    }

    pub async fn run(&'static self) -> Result<(), Box<dyn std::error::Error>> {
        // udp loop
        tokio::spawn(async move {
            match self.run_udp_loop().await {
                Ok(()) => {}
                Err(err) => {
                    error!("udp loop returned error: {err}");
                }
            }
        });

        loop {
            match self.tcp_socket.accept().await {
                Ok((socket, peer)) => {
                    let mut client = RelayClient::new(socket, self.udp_socket.clone(), peer);

                    tokio::spawn(async move {
                        match tokio::time::timeout(Duration::from_secs(45), self.handle_new_conn(&mut client)).await {
                            Ok(Ok(())) => {
                                info!(
                                    "[{}] Relay login successful, relaying to {}",
                                    peer,
                                    client.get_relay_target().unwrap()
                                );

                                let client = Arc::new(client);

                                self.clients
                                    .lock()
                                    .insert(*client.get_udp_peer().unwrap(), client.clone());

                                tokio::spawn(async move {
                                    match RelayClient::run_client(&client).await {
                                        Ok(()) => {}

                                        Err(err) => {
                                            warn!(
                                                "[{}] client terminated due to an error: {}",
                                                client.get_peer_addr(),
                                                err
                                            );
                                        }
                                    }

                                    self.clients.lock().remove(client.get_udp_peer().unwrap());
                                });
                            }

                            Ok(Err(err)) => {
                                warn!("[{peer}] Error during relay login: {err}");

                                let _ = tokio::time::timeout(
                                    Duration::from_secs(10),
                                    client.send_relay_error(&err.to_string()),
                                )
                                .await;
                            }

                            Err(_) => {
                                warn!("[{peer}] Timed out waiting for relay login, disconnecting");
                            }
                        }
                    });
                }

                Err(e) => {
                    error!("failed to accept new connection: {e}");
                }
            }
        }
    }

    pub async fn run_udp_loop(&'static self) -> Result<(), Box<dyn std::error::Error>> {
        let mut buf = [0u8; 65536];

        loop {
            let (size, peer) = self.udp_socket.recv_from(&mut buf).await?;

            if size == 8 {
                // possible that this is a message for the relay!
                let mut small_buf = [0u8; 4];
                small_buf.copy_from_slice(&buf[0..4]); // idk how to avoid this lol

                if RELAY_MAGIC == u32::from_be_bytes(small_buf) {
                    small_buf.copy_from_slice(&buf[4..8]);
                    let udp_id = u32::from_be_bytes(small_buf);

                    let awaiters = self.udp_awaiters.lock();
                    let mut accqueue = self.udp_accepted_queue.lock();

                    if let Some(notify) = awaiters.get(&udp_id) {
                        notify.notify_one();
                        accqueue.push((udp_id, peer));
                        continue;
                    }
                }
            }

            // if we got here, this is most likely a regular message, relay it
            let cl = self.clients.lock().get(&peer).cloned();

            if let Some(cl) = cl {
                match cl.push_new_udp_message(&buf[..size]).await {
                    Ok(()) => {}

                    Err(err) => {
                        warn!("[{peer}] failed to relay message to server: {err}");
                    }
                }
            }
        }
    }

    pub async fn handle_new_conn(&'static self, client: &mut RelayClient) -> Result<(), RelayError> {
        let host = client.wait_for_relay_login().await?;

        if !self.servers.contains(&host) {
            warn!(
                "[{}] Client tried to connect to disallowed host: {}",
                client.get_peer_addr(),
                host
            );

            return Err(RelayError::DisallowedHost);
        }

        client.set_relay_target(Some(host));
        let udp_id = client.send_udp_question().await?;

        debug!(
            "[{}] sent udp id {udp_id}, waiting for response..",
            client.get_peer_addr()
        );

        let notify = Arc::new(Notify::new());
        self.udp_awaiters.lock().insert(udp_id, notify.clone());

        notify.notified().await; // wait for a udp connection to claim us

        let udp_peer;
        {
            let mut queue = self.udp_accepted_queue.lock();
            let idx = queue.iter().position(|(id, _v)| *id == udp_id);

            if idx.is_none() {
                warn!("[{}] failed to find the ip in accepted queue!!", client.get_peer_addr());
                return Err(RelayError::InternalError);
            }

            (_, udp_peer) = queue.remove(idx.unwrap());
        }

        // we now have a udp ip
        debug!("[{}] linked to UDP address: {}", client.get_peer_addr(), udp_peer);
        client.set_udp_peer(Some(udp_peer));

        client.establish_upstream_connection().await.inspect_err(|e| {
            warn!(
                "[{}] error during establishing upstream connection: {e}",
                client.get_peer_addr()
            );
        })?;

        client.send_relay_established().await?;

        Ok(())
    }
}
