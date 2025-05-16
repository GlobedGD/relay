use std::net::SocketAddr;

use logger::{LogLevelFilter, Logger, get_log_level, info, log};
use relay_server::RelayServer;

mod logger;
mod relay_client;
mod relay_server;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let logger = Logger::instance("globed_relay", true);

    log::set_logger(logger).expect("failed to set logger");
    log::set_max_level(get_log_level("GLOBED_RELAY_LOG_LEVEL").unwrap_or({
        #[cfg(debug_assertions)]
        {
            LogLevelFilter::Trace
        }
        #[cfg(not(debug_assertions))]
        {
            LogLevelFilter::Info
        }
    }));

    let mut args = std::env::args();
    let _ = args.next();

    let host_addr = args
        .next()
        .expect("server listening address expected as the first argument")
        .parse::<SocketAddr>()
        .expect("invalid server listen address");

    let mut servers = Vec::new();

    for host in args {
        servers.push(host.parse::<SocketAddr>().expect("Failed to parse address"));
    }

    info!("Starting server on {host_addr}");
    info!("Permitted hosts:");
    for host in &servers {
        info!("- {host}");
    }

    let server = Box::leak(Box::new(
        RelayServer::new(host_addr, servers)
            .await
            .expect("failed to create server"),
    ));
    server.run().await?;

    Ok(())
}
