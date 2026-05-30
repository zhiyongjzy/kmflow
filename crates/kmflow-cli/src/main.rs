use anyhow::Result;
use clap::{Parser, Subcommand};
use kmflow_daemon::ipc::{IpcClient, IpcRequest, IpcResponse};
use kmflow_proto::{DEFAULT_PORT, KmflowConfig};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "kmflow",
    version,
    about = "Keyboard & Mouse Flow — LAN KVM sharing"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Port for QUIC transport
    #[arg(long, global = true, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the KMFlow daemon
    Start {
        /// Remote peer is on the right
        #[arg(long, group = "edge")]
        right: bool,
        /// Remote peer is on the left
        #[arg(long, group = "edge")]
        left: bool,
        /// Remote peer is above
        #[arg(long, group = "edge")]
        top: bool,
        /// Remote peer is below
        #[arg(long, group = "edge")]
        bottom: bool,
        /// Override display scale factor (e.g. 2.0 for 200%)
        #[arg(long)]
        scale: Option<f64>,
    },
    /// Stop the running daemon
    Stop,
    /// Pair with a remote peer
    Pair {
        /// IP address or hostname of the peer
        addr: String,
    },
    /// Configure screen layout
    Layout {
        /// Peer on the right side
        #[arg(long)]
        right: Option<String>,
        /// Peer on the left side
        #[arg(long)]
        left: Option<String>,
        /// Peer above
        #[arg(long)]
        top: Option<String>,
        /// Peer below
        #[arg(long)]
        bottom: Option<String>,
    },
    /// Show connection status
    Status,
    /// Generate firewall rules
    SetupFirewall,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        "kmflow=debug,kmflow_daemon=debug,kmflow_net=debug,kmflow_input=debug"
    } else {
        "kmflow=info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .init();

    match cli.command {
        Commands::Start {
            right,
            left,
            top,
            bottom,
            scale,
        } => {
            let edge = if right {
                Some(kmflow_proto::Edge::Right)
            } else if left {
                Some(kmflow_proto::Edge::Left)
            } else if top {
                Some(kmflow_proto::Edge::Top)
            } else if bottom {
                Some(kmflow_proto::Edge::Bottom)
            } else {
                None
            };
            cmd_start(cli.port, edge, scale).await
        }
        Commands::Stop => cmd_stop().await,
        Commands::Pair { addr } => cmd_pair(addr).await,
        Commands::Layout {
            right,
            left,
            top,
            bottom,
        } => cmd_layout(right, left, top, bottom).await,
        Commands::Status => cmd_status().await,
        Commands::SetupFirewall => cmd_setup_firewall(cli.port),
    }
}

async fn cmd_start(port: u16, edge: Option<kmflow_proto::Edge>, scale: Option<f64>) -> Result<()> {
    println!("Starting KMFlow daemon on port {port}...");

    let config = load_config()?;
    let config = KmflowConfig {
        port,
        layout: if let Some(e) = edge {
            vec![kmflow_proto::LayoutEntry {
                peer_hostname: String::new(),
                edge: e,
            }]
        } else {
            config.layout
        },
        scale_override: scale,
        ..config
    };

    let daemon = kmflow_daemon::Daemon::new(config).await?;

    tokio::select! {
        result = daemon.run() => {
            result?;
        }
        _ = tokio::signal::ctrl_c() => {
            println!("\nShutting down...");
            daemon.shutdown();
        }
    }

    Ok(())
}

async fn cmd_stop() -> Result<()> {
    let response = IpcClient::send(&IpcRequest::Stop).await?;
    match response {
        IpcResponse::Ok { message } => println!("{message}"),
        IpcResponse::Error { message } => eprintln!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

async fn cmd_pair(addr: String) -> Result<()> {
    let response = IpcClient::send(&IpcRequest::Pair { addr: addr.clone() }).await?;
    match response {
        IpcResponse::Ok { message } => println!("{message}"),
        IpcResponse::Error { message } => eprintln!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

async fn cmd_layout(
    right: Option<String>,
    left: Option<String>,
    top: Option<String>,
    bottom: Option<String>,
) -> Result<()> {
    let (peer, edge) = if let Some(p) = right {
        (p, "right")
    } else if let Some(p) = left {
        (p, "left")
    } else if let Some(p) = top {
        (p, "top")
    } else if let Some(p) = bottom {
        (p, "bottom")
    } else {
        anyhow::bail!("specify at least one direction (--right, --left, --top, --bottom)");
    };

    let response = IpcClient::send(&IpcRequest::Layout {
        peer,
        edge: edge.to_string(),
    })
    .await?;

    match response {
        IpcResponse::Ok { message } => println!("{message}"),
        IpcResponse::Error { message } => eprintln!("Error: {message}"),
        _ => {}
    }
    Ok(())
}

async fn cmd_status() -> Result<()> {
    let response = IpcClient::send(&IpcRequest::Status).await?;
    match response {
        IpcResponse::Status { state, peers } => {
            println!("State: {state}");
            if peers.is_empty() {
                println!("No peers connected");
            } else {
                println!(
                    "{:<16} {:<18} {:<12} {:<8} SCREEN",
                    "HOSTNAME", "ADDRESS", "STATE", "RTT"
                );
                for p in peers {
                    println!(
                        "{:<16} {:<18} {:<12} {:<8} {}",
                        p.hostname,
                        p.addr,
                        p.state,
                        format!("{}ms", p.rtt_ms as u32),
                        p.screen
                    );
                }
            }
        }
        IpcResponse::Error { message } => {
            eprintln!("Error: {message}");
            eprintln!("Is the daemon running? Try: kmflow start");
        }
        _ => {}
    }
    Ok(())
}

fn cmd_setup_firewall(port: u16) -> Result<()> {
    println!("# KMFlow firewall rules");
    println!("# Run the appropriate command for your system:\n");
    println!("# UFW (Ubuntu/Debian):");
    println!("sudo ufw allow {port}/udp comment 'KMFlow QUIC'\n");
    println!("# firewalld (Fedora/RHEL):");
    println!("sudo firewall-cmd --permanent --add-port={port}/udp");
    println!("sudo firewall-cmd --reload\n");
    println!("# iptables:");
    println!("sudo iptables -A INPUT -p udp --dport {port} -j ACCEPT");
    Ok(())
}

fn load_config() -> Result<KmflowConfig> {
    let config_dir = directories::ProjectDirs::from("", "", "kmflow")
        .map(|d| d.config_dir().to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from(".kmflow"));

    let config_path = config_dir.join("config.toml");
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)?;
        Ok(toml::from_str(&content)?)
    } else {
        Ok(KmflowConfig::default())
    }
}
