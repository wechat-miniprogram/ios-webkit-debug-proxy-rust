// iOS WebKit Debug Proxy - Rust rewrite
// Based on Google BSD license https://developers.google.com/google-bsd-license
// Original author Copyright 2012 Google Inc. wrightt@google.com

use clap::Parser;
use ios_webkit_debug_proxy::{HttpServiceConfig, ProxyBridge};
use log::{error, info};
use tokio_util::sync::CancellationToken;

mod port_config;

/// iOS WebKit Remote Debugging Protocol Proxy (Rust rewrite)
#[derive(Parser, Debug)]
#[command(name = "ios_webkit_debug_proxy", version, about)]
struct Args {
    /// Target device UDID[:minPort-[maxPort]]
    #[arg(short = 'u', long = "udid")]
    udid: Option<String>,

    /// UDID to port configuration (CSV format)
    #[arg(short = 'c', long = "config")]
    config: Option<String>,

    /// DevTools frontend UI path or URL
    #[arg(short = 'f', long = "frontend")]
    frontend: Option<String>,

    /// Disable DevTools frontend
    #[arg(short = 'F', long = "no-frontend")]
    no_frontend: bool,

    /// Simulator WebInspector socket address
    #[arg(short = 's', long = "simulator-webinspector")]
    sim_wi_socket_addr: Option<String>,
}

#[tokio::main]
async fn main() {
    env_logger::init();
    let args = Args::parse();

    // Parse configuration
    let default_config = "null:9221,:9222-9322";
    let default_frontend =
        "http://chrome-devtools-frontend.appspot.com/static/27.0.1453.93/devtools.html";

    let config = if let Some(ref udid) = args.udid {
        // Validate UDID format
        let re = regex::Regex::new(r"^[a-fA-F0-9-]{25,}(:[0-9]*(-[0-9]+)?)?$").unwrap();
        if !re.is_match(udid) {
            error!("Invalid UDID format: {}", udid);
            std::process::exit(2);
        }
        let has_port = udid.contains(':');
        if has_port {
            udid.clone()
        } else {
            format!("{}:", udid)
        }
    } else {
        args.config.unwrap_or_else(|| default_config.to_string())
    };

    let frontend = if args.no_frontend {
        None
    } else {
        Some(
            args.frontend
                .unwrap_or_else(|| default_frontend.to_string()),
        )
    };

    let sim_wi_socket_addr = args.sim_wi_socket_addr.as_ref().map(|x| x.as_str());

    info!("iOS WebKit Debug Proxy starting...");
    info!("Config: {}", config);
    if let Some(ref fe) = frontend {
        info!("Frontend: {}", fe);
    }

    // Parse port configuration
    let mut pc = port_config::PortConfig::new();
    if pc.add_line(&config).is_some() {
        pc.clear();
        let _ = pc.add_file(&config);
    }

    // Get device list port
    let mut list_port: i32 = -1;
    let mut list_min: i32 = -1;
    let mut list_max: i32 = -1;
    if pc
        .select_port(None, &mut list_port, &mut list_min, &mut list_max)
        .is_ok()
        && list_port < 0
        && list_min > 0
    {
        list_port = list_min;
    }

    // Get device port range
    let mut dev_port: i32 = -1;
    let mut dev_min: i32 = -1;
    let mut dev_max: i32 = -1;
    pc.select_port(Some("*"), &mut dev_port, &mut dev_min, &mut dev_max)
        .ok();

    let device_ports = if dev_min > 0 && dev_max >= dev_min {
        Some((dev_min as u16)..(dev_max as u16 + 1))
    } else {
        None
    };
    let http_service_config = if list_port > 0 {
        Some(HttpServiceConfig {
            device_list_port: list_port as u16,
            frontend,
        })
    } else {
        None
    };

    // Create cancellation token
    let cancellation = CancellationToken::new();
    let cancel_on_signal = cancellation.clone();

    // Register signal handler
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        info!("Received interrupt signal, shutting down...");
        cancel_on_signal.cancel();
    });

    // Create and run proxy
    let mut bridge = ProxyBridge::new(device_ports, http_service_config, sim_wi_socket_addr, None);

    if let Err(e) = bridge.run(cancellation).await {
        error!("Proxy runtime error: {}", e);
        std::process::exit(1);
    }

    info!("Proxy has been shut down");
}
