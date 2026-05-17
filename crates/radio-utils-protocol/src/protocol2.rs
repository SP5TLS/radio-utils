use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use num_complex::Complex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::discovery;
use crate::transport::UdpTransport;
use crate::types::*;

// Host -> Radio ports (fixed on the radio side)
const PORT_GENERAL: u16 = 1024;
const PORT_RX_SPECIFIC: u16 = 1025;
#[allow(dead_code)]
const PORT_TX_SPECIFIC: u16 = 1026;
const PORT_HIGH_PRIORITY: u16 = 1027;
const PORT_TX_IQ: u16 = 1029;

const SAMPLES_PER_DDC_PACKET: usize = 238;

/// Commands that can be sent to a running Protocol 2 client.
#[derive(Debug)]
pub enum P2Command {
    SetRxFrequency(usize, u32),
    SetTxFrequency(u32),
    SetSampleRate(u32),
    SetTxDrive(u8),
    SetPtt(bool),
    Stop,
}

/// Protocol 2 client for connecting to HPSDR hardware.
pub struct Protocol2Client {
    // Transport sockets (multi-port, all ephemeral)
    general_transport: UdpTransport,
    rx_specific_transport: UdpTransport,
    _tx_specific_transport: UdpTransport,
    hp_transport: UdpTransport,
    tx_iq_transport: UdpTransport,
    // HP status receive socket (separate from rx_specific)
    hp_status_transport: UdpTransport,
    // DDC IQ receive sockets (one per DDC, ephemeral ports)
    ddc_transports: Vec<UdpTransport>,

    device: DiscoveredDevice,
    running: bool,

    // Configuration
    sample_rate: u32,
    nddc: u8,
    rx_frequencies: [u32; 12],
    tx_frequency: u32,
    tx_drive: u8,
    ptt: bool,

    // Sequence tracking
    hp_seq: u32,
    general_seq: u32,
    rx_specific_seq: u32,
    tx_iq_seq: u32,

    // Pre-allocated send buffers (avoids per-call allocation)
    hp_pkt_buf: Vec<u8>,
    tx_iq_pkt_buf: Vec<u8>,

    // Channels
    iq_senders: HashMap<usize, mpsc::Sender<Vec<Complex<f64>>>>,
    status_sender: mpsc::Sender<RadioStatus>,
    status_receiver: Option<mpsc::Receiver<RadioStatus>>,
    cmd_sender: mpsc::Sender<P2Command>,
    cmd_receiver: Option<mpsc::Receiver<P2Command>>,
}

impl Protocol2Client {
    /// Discover all HPSDR devices on the network using Protocol 2.
    pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        discovery::discover_on_interfaces(
            build_p2_discovery_request,
            parse_p2_discovery_response,
            PORT_GENERAL,
            timeout,
        )
        .await
    }

    /// Send a discovery request to a specific address (unicast).
    pub async fn discover_at(addr: Ipv4Addr, timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        discovery::discover_at_addr(
            build_p2_discovery_request,
            parse_p2_discovery_response,
            addr,
            PORT_GENERAL,
            timeout,
        )
        .await
    }

    /// Connect to a discovered Protocol 2 device.
    pub async fn connect(device: &DiscoveredDevice, nddc: u8) -> Result<Self> {
        let general_transport = UdpTransport::bind_any().await?;
        let rx_specific_transport = UdpTransport::bind_any().await?;
        let tx_specific_transport = UdpTransport::bind_any().await?;
        let hp_transport = UdpTransport::bind_any().await?;
        let tx_iq_transport = UdpTransport::bind_any().await?;
        let hp_status_transport = UdpTransport::bind_any().await?;

        // Bind DDC receive sockets on ephemeral ports (no fixed port conflicts)
        let mut ddc_transports = Vec::new();
        for _ in 0..nddc {
            let t = UdpTransport::bind_any().await?;
            ddc_transports.push(t);
        }

        let (status_tx, status_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);

        Ok(Self {
            general_transport,
            rx_specific_transport,
            _tx_specific_transport: tx_specific_transport,
            hp_transport,
            tx_iq_transport,
            hp_status_transport,
            ddc_transports,
            device: device.clone(),
            running: false,
            sample_rate: 192_000,
            nddc,
            rx_frequencies: [7_074_000; 12],
            tx_frequency: 7_074_000,
            tx_drive: 128,
            ptt: false,
            hp_seq: 0,
            general_seq: 0,
            rx_specific_seq: 0,
            tx_iq_seq: 0,
            hp_pkt_buf: vec![0u8; 1444],
            tx_iq_pkt_buf: vec![0u8; 1444],
            iq_senders: HashMap::new(),
            status_sender: status_tx,
            status_receiver: Some(status_rx),
            cmd_sender: cmd_tx,
            cmd_receiver: Some(cmd_rx),
        })
    }

    /// Get a command sender for controlling the client while running.
    pub fn command_sender(&self) -> mpsc::Sender<P2Command> {
        self.cmd_sender.clone()
    }

    /// Take the status stream receiver.
    pub fn status_stream(&mut self) -> Option<mpsc::Receiver<RadioStatus>> {
        self.status_receiver.take()
    }

    /// Start streaming.
    pub async fn start(&mut self) -> Result<()> {
        // Send general config packet
        self.send_general_config().await?;

        // Send RX-specific config
        self.send_rx_config().await?;

        // Send high-priority command with run=true
        self.running = true;
        self.send_high_priority().await?;

        log::debug!("P2 Started streaming");
        Ok(())
    }

    /// Stop streaming.
    pub async fn stop(&mut self) -> Result<()> {
        self.running = false;
        self.send_high_priority().await?;
        log::debug!("P2 Stopped streaming");
        Ok(())
    }

    // -- Configuration commands -----------------------------------------------

    pub fn set_rx_frequency(&mut self, ddc: usize, freq_hz: u32) {
        if ddc < self.rx_frequencies.len() {
            self.rx_frequencies[ddc] = freq_hz;
        }
    }

    pub fn set_tx_frequency(&mut self, freq_hz: u32) {
        self.tx_frequency = freq_hz;
    }

    pub fn set_sample_rate(&mut self, rate: u32) {
        self.sample_rate = rate;
    }

    pub fn set_tx_drive(&mut self, drive: u8) {
        self.tx_drive = drive;
    }

    pub fn set_ptt(&mut self, on: bool) {
        self.ptt = on;
    }

    /// Get a receiver for IQ data from a specific DDC.
    pub fn rx_iq_stream(&mut self, ddc: usize) -> mpsc::Receiver<Vec<Complex<f64>>> {
        let (tx, rx) = mpsc::channel(256);
        self.iq_senders.insert(ddc, tx);
        rx
    }

    /// Send a high-priority command update (frequencies, PTT, run).
    pub async fn send_high_priority(&mut self) -> Result<()> {
        self.hp_pkt_buf.fill(0);
        let pkt = &mut self.hp_pkt_buf;

        // Sequence number
        let seq = self.hp_seq;
        self.hp_seq = self.hp_seq.wrapping_add(1);
        pkt[0..4].copy_from_slice(&seq.to_be_bytes());

        // Byte 4: run (bit 0), PTT (bit 1)
        let mut flags = 0u8;
        if self.running {
            flags |= 0x01;
        }
        if self.ptt {
            flags |= 0x02;
        }
        pkt[4] = flags;

        // RX frequencies: bytes 9-56 (12 x 4 bytes)
        for i in 0..12 {
            let off = 9 + i * 4;
            pkt[off..off + 4].copy_from_slice(&self.rx_frequencies[i].to_be_bytes());
        }

        // TX frequency at byte 329
        if pkt.len() > 333 {
            pkt[329..333].copy_from_slice(&self.tx_frequency.to_be_bytes());
        }

        // TX drive at byte 345
        if pkt.len() > 345 {
            pkt[345] = self.tx_drive;
        }

        let addr = SocketAddr::new(self.device.addr.ip(), PORT_HIGH_PRIORITY);
        self.hp_transport.send_to(pkt, addr).await?;
        Ok(())
    }

    /// Send general configuration packet (port 1024).
    ///
    /// Advertises the actual ephemeral ports we're listening on so the radio
    /// sends data to the right place (fixes I1 and I4).
    async fn send_general_config(&mut self) -> Result<()> {
        let mut pkt = vec![0u8; 60];

        let seq = self.general_seq;
        self.general_seq = self.general_seq.wrapping_add(1);
        pkt[0..4].copy_from_slice(&seq.to_be_bytes());

        pkt[4] = 0x00; // general config command

        // Port assignments — use actual local ports of our transports
        let rx_specific_port = self.rx_specific_transport.local_addr()?.port();
        let tx_specific_port = self._tx_specific_transport.local_addr()?.port();
        let hp_from_pc_port = self.hp_transport.local_addr()?.port();
        let hp_to_pc_port = self.hp_status_transport.local_addr()?.port();
        let tx_iq_port = self.tx_iq_transport.local_addr()?.port();

        pkt[5..7].copy_from_slice(&rx_specific_port.to_be_bytes());
        pkt[7..9].copy_from_slice(&tx_specific_port.to_be_bytes());
        pkt[9..11].copy_from_slice(&hp_from_pc_port.to_be_bytes());
        pkt[11..13].copy_from_slice(&hp_to_pc_port.to_be_bytes());
        // TODO: bytes 13-14 — verify against OpenHPSDR P2 spec
        // (likely the audio/wideband port; no transport exists yet)
        pkt[15..17].copy_from_slice(&tx_iq_port.to_be_bytes());

        // DDC port assignments — advertise all DDC ports
        for (i, ddc) in self.ddc_transports.iter().enumerate() {
            let ddc_port = ddc.local_addr()?.port();
            let offset = 17 + i * 2;
            if offset + 2 <= pkt.len() {
                pkt[offset..offset + 2].copy_from_slice(&ddc_port.to_be_bytes());
            }
        }

        let addr = SocketAddr::new(self.device.addr.ip(), PORT_GENERAL);
        self.general_transport.send_to(&pkt, addr).await?;
        Ok(())
    }

    /// Send RX-specific configuration (port 1025).
    async fn send_rx_config(&mut self) -> Result<()> {
        let mut pkt = vec![0u8; 1444];

        let seq = self.rx_specific_seq;
        self.rx_specific_seq = self.rx_specific_seq.wrapping_add(1);
        pkt[0..4].copy_from_slice(&seq.to_be_bytes());

        // Byte 7: enabled receivers bitmask
        let mut enabled_bits = 0u8;
        for i in 0..self.nddc.min(8) {
            enabled_bits |= 1 << i;
        }
        pkt[7] = enabled_bits;

        // Per-DDC sample rate in kHz (at offset 18 + ddc*6)
        let sr_khz = (self.sample_rate / 1000) as u16;
        for ddc in 0..self.nddc as usize {
            let off = 18 + ddc * 6;
            if off + 2 <= pkt.len() {
                pkt[off..off + 2].copy_from_slice(&sr_khz.to_be_bytes());
            }
        }

        let addr = SocketAddr::new(self.device.addr.ip(), PORT_RX_SPECIFIC);
        self.rx_specific_transport.send_to(&pkt, addr).await?;
        Ok(())
    }

    /// Send TX IQ data packet (port 1029).
    pub async fn send_tx_iq(&mut self, samples: &[Complex<f64>]) -> Result<()> {
        // 1444 bytes: 4-byte seq + 1440 bytes (240 samples x 6 bytes)
        let max_samples = 240;
        let n = samples.len().min(max_samples);
        if samples.len() > max_samples {
            log::debug!(
                "TX IQ truncated: {} samples exceeds max {}, dropping {}",
                samples.len(),
                max_samples,
                samples.len() - max_samples,
            );
        }

        self.tx_iq_pkt_buf.fill(0);
        let pkt = &mut self.tx_iq_pkt_buf;
        let seq = self.tx_iq_seq;
        self.tx_iq_seq = self.tx_iq_seq.wrapping_add(1);
        pkt[0..4].copy_from_slice(&seq.to_be_bytes());

        let mut offset = 4;
        for sample in samples.iter().take(n) {
            offset = pack_iq_24bit_into_negate_q(&mut *pkt, offset, *sample);
        }

        let addr = SocketAddr::new(self.device.addr.ip(), PORT_TX_IQ);
        self.tx_iq_transport.send_to(pkt, addr).await?;
        Ok(())
    }

    /// Handle a command from the command channel.
    async fn handle_command(&mut self, cmd: P2Command) {
        match cmd {
            P2Command::SetRxFrequency(ddc, freq) => {
                self.set_rx_frequency(ddc, freq);
                if let Err(e) = self.send_high_priority().await {
                    log::warn!("Failed to send HP after freq change: {}", e);
                }
            }
            P2Command::SetTxFrequency(freq) => {
                self.set_tx_frequency(freq);
                if let Err(e) = self.send_high_priority().await {
                    log::warn!("Failed to send HP after TX freq change: {}", e);
                }
            }
            P2Command::SetSampleRate(rate) => {
                self.set_sample_rate(rate);
                if let Err(e) = self.send_rx_config().await {
                    log::warn!("Failed to send RX config after sample rate change: {}", e);
                }
            }
            P2Command::SetTxDrive(drive) => {
                self.set_tx_drive(drive);
                if let Err(e) = self.send_high_priority().await {
                    log::warn!("Failed to send HP after drive change: {}", e);
                }
            }
            P2Command::SetPtt(on) => {
                self.set_ptt(on);
                if let Err(e) = self.send_high_priority().await {
                    log::warn!("Failed to send HP after PTT change: {}", e);
                }
            }
            P2Command::Stop => {
                if let Err(e) = self.stop().await {
                    log::warn!("Failed to send stop: {}", e);
                }
            }
        }
    }

    /// Run the main receive loop. Receives HP status and IQ from all DDCs.
    ///
    /// Each DDC transport is moved into its own spawned task that forwards
    /// received packets through a shared `mpsc` channel, so every DDC is
    /// monitored with equal priority regardless of count.
    pub async fn run(&mut self) -> Result<()> {
        let mut cmd_rx = self
            .cmd_receiver
            .take()
            .ok_or_else(|| ProtocolError::InvalidPacket("command receiver already taken".into()))?;

        // Move DDC transports out of self into per-DDC forwarding tasks.
        let ddc_transports = std::mem::take(&mut self.ddc_transports);
        let ddc_count = ddc_transports.len();

        // Merged channel: each DDC task sends (ddc_index, packet_data).
        let (ddc_tx, mut ddc_rx) = mpsc::channel::<(usize, Vec<u8>)>(256 * ddc_count.max(1));

        let mut ddc_tasks: Vec<JoinHandle<UdpTransport>> = Vec::with_capacity(ddc_count);
        for (idx, transport) in ddc_transports.into_iter().enumerate() {
            let tx = ddc_tx.clone();
            ddc_tasks.push(tokio::spawn(async move {
                let mut buf = vec![0u8; 2048];
                while let Ok((len, _)) = transport.recv_from(&mut buf).await {
                    if tx.send((idx, buf[..len].to_vec())).await.is_err() {
                        break; // receiver dropped, shutting down
                    }
                }
                transport // return transport so we can restore it
            }));
        }
        // Drop our copy so the channel closes when all tasks finish.
        drop(ddc_tx);

        let mut hp_buf = vec![0u8; 128];

        let result = loop {
            if !self.running {
                break Ok(());
            }

            tokio::select! {
                result = self.hp_status_transport.recv_from(&mut hp_buf) => {
                    match result {
                        Ok((len, _)) => self.parse_hp_status(&hp_buf[..len]),
                        Err(e) => break Err(e),
                    }
                }
                ddc_packet = ddc_rx.recv() => {
                    match ddc_packet {
                        Some((ddc_idx, data)) => {
                            self.parse_ddc_iq_packet(ddc_idx, &data);
                        }
                        None => break Ok(()), // all DDC tasks exited
                    }
                }
                cmd = cmd_rx.recv() => {
                    match cmd {
                        Some(c) => self.handle_command(c).await,
                        None => break Ok(()),
                    }
                }
            }
        };

        // Shut down DDC forwarding tasks and restore transports.
        for task in &ddc_tasks {
            task.abort();
        }
        let mut restored = Vec::with_capacity(ddc_count);
        for task in ddc_tasks {
            // task was aborted or panicked; transport is lost
            if let Ok(transport) = task.await {
                restored.push(transport);
            }
        }
        self.ddc_transports = restored;

        result
    }

    /// Parse high-priority status response (60 bytes from radio).
    fn parse_hp_status(&self, data: &[u8]) {
        if data.len() < 24 {
            return;
        }

        let mut status = RadioStatus {
            ptt: (data[4] & 0x01) != 0,
            adc_overflow: data[5],
            exciter_power: u16::from_be_bytes([data[6], data[7]]),
            ..Default::default()
        };
        if data.len() > 15 {
            status.forward_power = u16::from_be_bytes([data[14], data[15]]);
        }
        if data.len() > 23 {
            status.reverse_power = u16::from_be_bytes([data[22], data[23]]);
        }

        let _ = self.status_sender.try_send(status);
    }

    /// Parse a DDC IQ data packet (1444 bytes from radio).
    fn parse_ddc_iq_packet(&self, ddc: usize, data: &[u8]) {
        if data.len() < 16 {
            return;
        }

        // Header: seq(4) + timestamp(8) + bits_per_sample(2) + samples_per_frame(2)
        let _seq = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let samples_per_frame = u16::from_be_bytes([data[14], data[15]]) as usize;
        let n_samples = samples_per_frame.min(SAMPLES_PER_DDC_PACKET);

        let mut samples = Vec::with_capacity(n_samples);
        for i in 0..n_samples {
            let offset = 16 + i * 6;
            if offset + 6 <= data.len() {
                samples.push(unpack_iq_24bit(data, offset));
            }
        }

        if let Some(sender) = self.iq_senders.get(&ddc) {
            if !samples.is_empty() {
                let _ = sender.try_send(samples);
            }
        }
    }
}

/// Build a Protocol 2 discovery request packet (60 bytes).
fn build_p2_discovery_request() -> Vec<u8> {
    let mut req = vec![0u8; 60];
    // Bytes 0-3: zeros
    req[4] = 0x02; // discovery command
    req
}

/// Parse a Protocol 2 discovery response.
fn parse_p2_discovery_response(data: &[u8], addr: SocketAddr) -> Option<DiscoveredDevice> {
    if data.len() < 21 {
        return None;
    }

    // Bytes 0-3 should be zeros
    if data[0] != 0 || data[1] != 0 || data[2] != 0 || data[3] != 0 {
        return None;
    }

    let status = data[4];
    if status != 0x02 && status != 0x03 {
        return None;
    }

    let mut mac = [0u8; 6];
    mac.copy_from_slice(&data[5..11]);

    let board_type = data[11];
    let _protocol_version = data[12];
    let firmware_version = data[13];
    let num_rxs = data[20];

    let hw_type = HpsdrHw::from_p2_code(board_type);
    let radio_addr = SocketAddr::new(addr.ip(), PORT_GENERAL);

    Some(DiscoveredDevice {
        addr: radio_addr,
        mac,
        hw_type,
        firmware_version,
        protocol: Protocol::Protocol2,
        num_rxs,
        status,
    })
}
