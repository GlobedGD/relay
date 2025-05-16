use std::{fmt::Display, net::SocketAddr, sync::Arc, time::Duration};

use crate::logger::{debug, trace, warn};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpStream, UdpSocket,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::Mutex,
};

pub const RELAY_MAGIC: u32 = 0x7f8a9b0c;

pub struct RelayClient {
    downstream_write: Mutex<OwnedWriteHalf>,
    downstream_read: Mutex<OwnedReadHalf>,
    downstream_udp: Arc<UdpSocket>,
    peer_address: SocketAddr,
    relay_target: Option<SocketAddr>,
    udp_id: u32,
    peer_udp_address: Option<SocketAddr>,
    upstream_write: Mutex<Option<OwnedWriteHalf>>,
    upstream_read: Mutex<Option<OwnedReadHalf>>,
    upstream_udp: Option<UdpSocket>,
    initial_conn_marker: u8,
}

pub enum RelayError {
    Socket(std::io::Error),
    InvalidMagic,
    InvalidHost,
    DisallowedHost,
    InternalError,
    Closed,
    UpstreamSocketFail(std::io::Error),
    DownstreamSocketFail(std::io::Error),
}

impl From<std::io::Error> for RelayError {
    fn from(value: std::io::Error) -> Self {
        Self::Socket(value)
    }
}

impl Display for RelayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(err) => write!(f, "Socket error: {err}"),
            Self::InvalidMagic => write!(f, "Invalid magic sent by the client"),
            Self::InvalidHost => write!(f, "Invalid host was sent by the client"),
            Self::DisallowedHost => write!(f, "Disallowed host"),
            Self::InternalError => write!(f, "Internal error, check other logs for more information"),
            Self::Closed => write!(f, "One of the ends of the connection was closed"),
            Self::UpstreamSocketFail(e) => write!(f, "Upstream socket failure: {e}"),
            Self::DownstreamSocketFail(e) => write!(f, "Downstream socket failure: {e}"),
        }
    }
}

impl RelayClient {
    pub fn new(socket: TcpStream, udp_socket: Arc<UdpSocket>, peer_address: SocketAddr) -> Self {
        let _ = socket.set_linger(Some(Duration::from_secs(1)));

        let (r, w) = socket.into_split();

        Self {
            downstream_write: Mutex::new(w),
            downstream_read: Mutex::new(r),
            downstream_udp: udp_socket,
            peer_address,
            relay_target: None,
            udp_id: 0,
            peer_udp_address: None,
            upstream_write: Mutex::new(None),
            upstream_read: Mutex::new(None),
            upstream_udp: None,
            initial_conn_marker: 0,
        }
    }

    pub fn get_peer_addr(&self) -> SocketAddr {
        self.peer_address
    }

    pub fn get_relay_target(&self) -> Option<&SocketAddr> {
        self.relay_target.as_ref()
    }

    pub fn set_relay_target(&mut self, target: Option<SocketAddr>) {
        self.relay_target = target;
    }

    pub fn set_udp_peer(&mut self, addr: Option<SocketAddr>) {
        self.peer_udp_address = addr;
    }

    pub fn get_udp_peer(&self) -> Option<&SocketAddr> {
        self.peer_udp_address.as_ref()
    }

    pub async fn wait_for_relay_login(&mut self) -> Result<SocketAddr, RelayError> {
        let mut socket = self.downstream_read.lock().await;

        self.initial_conn_marker = socket.read_u8().await?;

        let rmagic = socket.read_u32().await?;

        if rmagic != RELAY_MAGIC {
            return Err(RelayError::InvalidMagic);
        }

        let hostlen = socket.read_u32().await?;
        let mut buf = [0u8; 64];

        if hostlen > 64 {
            return Err(RelayError::InvalidHost);
        }

        socket.read_exact(&mut buf[..hostlen as usize]).await?;

        let host = String::from_utf8_lossy(&buf[..hostlen as usize]).into_owned();

        host.parse::<SocketAddr>().map_err(|_| RelayError::InvalidHost)
    }

    pub async fn send_udp_question(&mut self) -> Result<u32, RelayError> {
        let mut num = rand::random::<u32>();

        if num == 0 {
            num = 1; // hey :)
        }

        self.udp_id = num;

        let mut wr = self.downstream_write.lock().await;
        wr.write_u32(5).await?;
        wr.write_u8(1).await?;
        wr.write_u32(num).await?;

        Ok(num)
    }

    pub async fn send_relay_established(&mut self) -> Result<(), RelayError> {
        self.downstream_write.lock().await.write_u8(1).await?;

        Ok(())
    }

    pub async fn send_relay_error(&self, msg: &str) -> Result<(), RelayError> {
        let mut wr = self.downstream_write.lock().await;

        wr.write_u32(1 + 4 + msg.len() as u32).await?;
        wr.write_u8(0).await?;
        wr.write_u32(msg.len() as u32).await?;
        wr.write_all(msg.as_bytes()).await?;

        Ok(())
    }

    pub async fn establish_upstream_connection(&mut self) -> Result<(), RelayError> {
        let target = self.relay_target.as_ref().unwrap();
        trace!("[{}] connecting to upstream: {}", self.get_peer_addr(), target);

        let tcp = TcpStream::connect(target).await?;
        let udp = UdpSocket::bind("0.0.0.0:0").await?;

        let (r, mut w) = tcp.into_split();

        w.write_u8(self.initial_conn_marker).await?;

        *self.upstream_read.lock().await = Some(r);
        *self.upstream_write.lock().await = Some(w);
        self.upstream_udp = Some(udp);

        Ok(())
    }

    // up
    async fn run_tcp_send_loop(&self) -> Result<(), RelayError> {
        let mut buf = [0u8; 8192];

        loop {
            let n = self
                .downstream_read
                .lock()
                .await
                .read(&mut buf)
                .await
                .inspect_err(|e| {
                    #[cfg(debug_assertions)]
                    debug!("[{}] downstream tcp read error: {}", self.get_peer_addr(), e);
                })
                .map_err(RelayError::DownstreamSocketFail)?;

            if n == 0 {
                return Err(RelayError::Closed);
            }

            #[cfg(debug_assertions)]
            trace!("[{}] relaying {} tcp bytes UPstream", self.get_peer_addr(), n);

            self.upstream_write
                .lock()
                .await
                .as_mut()
                .unwrap()
                .write_all(&buf[..n])
                .await
                .map_err(RelayError::UpstreamSocketFail)?;
        }
    }

    // down
    async fn run_tcp_recv_loop(&self) -> Result<(), RelayError> {
        let mut buf = [0u8; 8192];

        loop {
            let n = self
                .upstream_read
                .lock()
                .await
                .as_mut()
                .unwrap()
                .read(&mut buf)
                .await
                .inspect_err(|e| {
                    #[cfg(debug_assertions)]
                    debug!("[{}] upstream tcp read error: {}", self.get_peer_addr(), e);
                })
                .map_err(RelayError::UpstreamSocketFail)?;

            if n == 0 {
                return Err(RelayError::Closed);
            }

            #[cfg(debug_assertions)]
            trace!("[{}] relaying {} tcp bytes DOWNstream", self.get_peer_addr(), n);

            self.downstream_write
                .lock()
                .await
                .write_all(&buf[..n])
                .await
                .map_err(RelayError::DownstreamSocketFail)?;
        }
    }

    // udp

    async fn run_udp_recv_loop(&self) -> Result<(), RelayError> {
        let mut buf = [0u8; 65536];

        loop {
            let (n, addr) = self.upstream_udp.as_ref().unwrap().recv_from(&mut buf).await?;

            if addr != self.relay_target.unwrap() {
                warn!(
                    "[{}] udp packet from unexpected target, not our relay target: {}",
                    self.get_relay_target().unwrap(),
                    addr
                );

                continue;
            }

            #[cfg(debug_assertions)]
            trace!("[{}] relaying {} udp bytes DOWNstream", self.get_peer_addr(), n);

            self.downstream_udp
                .send_to(&buf[..n], self.peer_udp_address.unwrap())
                .await?;
        }
    }

    pub async fn run_client(cl: &RelayClient) -> Result<(), RelayError> {
        tokio::select! {
            r = cl.run_tcp_send_loop() => r,
            r = cl.run_tcp_recv_loop() => r,
            r = cl.run_udp_recv_loop() => r,
        }
    }

    pub async fn push_new_udp_message(&self, message: &[u8]) -> Result<(), RelayError> {
        #[cfg(debug_assertions)]
        trace!(
            "[{}] relaying {} udp bytes UPstream",
            self.get_peer_addr(),
            message.len()
        );

        self.upstream_udp
            .as_ref()
            .unwrap()
            .send_to(message, self.relay_target.as_ref().unwrap())
            .await?;

        Ok(())
    }
}
