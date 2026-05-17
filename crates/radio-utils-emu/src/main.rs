//! OpenHPSDR Protocol 1 radio emulator.
//!
//! Emulates Hermes or Hermes Lite 2 hardware over UDP so SDR applications
//! like Thetis and deskHPSDR can discover and connect to it. Generates test
//! tone + noise IQ data on all RX DDC channels and supports echo mode for
//! TX→RX loopback testing.

use std::sync::Arc;

use clap::Parser;
use tokio::sync::RwLock;

use radio_utils_emu::protocol1::SiggenConfig;
use radio_utils_emu::radio::{EchoBuffer, EchoMode, HpsdrHw, HwInfo};

#[derive(Parser)]
#[command(
    name = "radio-utils-emu",
    about = "OpenHPSDR Protocol 1 radio emulator (Hermes / Hermes Lite 2)"
)]
struct Cli {
    /// Radio hardware type. One of `hermes` or `hermeslite` (alias: `hermeslite2`).
    #[arg(long, value_parser = parse_radio)]
    radio: HpsdrHw,

    /// MAC address (hex, e.g. 00:1c:c0:a2:22:5e)
    #[arg(long)]
    mac: Option<String>,

    /// Noise level as fraction of full-scale
    #[arg(long, default_value = "1.26e-5")]
    noise: f64,

    /// Enable echo mode: TX IQ is recorded and looped back on RX
    #[arg(long)]
    echo: bool,

    /// Enable live echo: TX IQ appears on RX in near real-time (plays once, not looped)
    #[arg(long)]
    echo_live: bool,

    /// Bind to a specific local IP (e.g. 192.168.1.100).
    /// Defaults to 0.0.0.0 (all interfaces). Use this when deskhpsdr
    /// discovers the emulator on the wrong network interface (VPN, etc.).
    #[arg(long, default_value = "0.0.0.0")]
    bind: String,

    /// Maximum concurrent Protocol-1 client sessions. Hosted multi-user
    /// deployments (proxy + emulator on one server) can raise this; firmware /
    /// LAN scenarios typically don't need more than a handful.
    #[arg(long, default_value_t = radio_utils_emu::protocol1::DEFAULT_MAX_CLIENTS)]
    max_clients: usize,

    /// Enable debug logging
    #[arg(short, long)]
    verbose: bool,
}

fn parse_radio(s: &str) -> Result<HpsdrHw, String> {
    HpsdrHw::from_name(s).ok_or_else(|| {
        format!(
            "unknown radio '{}'. Valid: {}",
            s,
            HpsdrHw::all_names().join(", ")
        )
    })
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // Init logging
    let log_level = if cli.verbose { "debug" } else { "info" };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level))
        .format_timestamp_secs()
        .init();

    // MAC address: stable default, overridable via --mac
    const DEFAULT_MAC: [u8; 6] = [0x02, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
    let mac = if let Some(ref mac_str) = cli.mac {
        let hex: String = mac_str.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        if hex.len() != 12 {
            eprintln!("MAC address must be 6 bytes (12 hex digits)");
            std::process::exit(1);
        }
        let mut bytes = [0u8; 6];
        for i in 0..6 {
            bytes[i] = match u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16) {
                Ok(b) => b,
                Err(_) => {
                    eprintln!("Invalid hex in MAC address");
                    std::process::exit(1);
                }
            };
        }
        bytes
    } else {
        DEFAULT_MAC
    };

    let sample_rate: u32 = 48000;

    let hw = Arc::new(HwInfo::new(cli.radio, mac));

    let sg_cfg = Arc::new(RwLock::new(SiggenConfig {
        sample_rate,
        noise_level: cli.noise,
    }));

    let echo_mode = if cli.echo_live {
        Some(EchoMode::Live)
    } else if cli.echo {
        Some(EchoMode::Loop)
    } else {
        None
    };

    let echo = Arc::new(RwLock::new(echo_mode.map(EchoBuffer::new)));

    let echo_label = match echo_mode {
        Some(EchoMode::Loop) => "loop",
        Some(EchoMode::Live) => "live",
        None => "off",
    };

    log::info!(
        "Starting HPSDR emulator: radio={}, noise={:.2e}, echo={}, max_clients={}",
        cli.radio,
        cli.noise,
        echo_label,
        cli.max_clients,
    );

    tokio::select! {
        result = radio_utils_emu::protocol1::run_protocol1(hw, sg_cfg, echo, &cli.bind, cli.max_clients) => {
            if let Err(e) = result {
                eprintln!("Fatal: failed to start Protocol 1: {}", e);
                std::process::exit(1);
            }
        }
        _ = tokio::signal::ctrl_c() => {
            log::info!("Shutting down...");
        }
    }
}
