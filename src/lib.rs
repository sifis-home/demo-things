use std::net::SocketAddr;

use clap::Parser;
use tracing_subscriber::filter::EnvFilter;

/// SIFIS-Home wot-rust demo thing
///
/// It sets up a servient listening the requested port and address and
/// advertises itself via mDNS/DNS-SD.
#[derive(Debug, Parser)]
pub struct CliCommon {
    /// Listening port
    #[arg(short, long, default_value = "3000")]
    pub listen_port: u16,

    /// Binding address
    #[arg(short = 'a', long, default_value = "0.0.0.0")]
    pub bind_addr: std::net::Ipv4Addr,

    /// Verbosity, more output per occurrence
    #[arg(
        long,
        short = 'v',
        action = clap::ArgAction::Count,
        global = true,
    )]
    pub verbose: u8,
}

impl CliCommon {
    pub fn setup_tracing(&self) {
        let filter = match self.verbose {
            0 => "error",
            1 => "warn",
            2 => "info",
            3 => "debug",
            _ => "trace",
        };
        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(filter))
            .unwrap();

        tracing_subscriber::fmt().with_env_filter(filter).init()
    }

    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::from((self.bind_addr, self.listen_port))
    }
}
