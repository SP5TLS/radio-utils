#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
use std::net::SocketAddr;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;

use num_complex::Complex;
#[cfg(not(target_arch = "wasm32"))]
use tokio::net::UdpSocket;
use tokio::sync::RwLock;
#[cfg(not(target_arch = "wasm32"))]
use tokio::sync::{mpsc, oneshot};

use crate::radio::{
    code_to_sample_rate, pack_iq_24bit_into, unpack_tx_iq_16bit_into, EchoBuffer, EchoMode,
    EchoPlaybackState, HwInfo, SignalGenerator,
};

#[cfg(not(target_arch = "wasm32"))]
const PORT: u16 = 1024;
const PACKET_SIZE: usize = 1032;
const SUBFRAME_SIZE: usize = 512;
const SYNC: [u8; 3] = [0x7F, 0x7F, 0x7F];

/// Default cap on concurrent Protocol-1 client sessions when an embedder doesn't
/// override it. Sized for hosted multi-user "virtual band" deployments rather than
/// the original 8-client desktop assumption.
pub const DEFAULT_MAX_CLIENTS: usize = 32;

/// Maximum TX IQ samples per sub-frame (504 bytes / 8 bytes per block).
const MAX_TX_IQ_SAMPLES: usize = 63;

/// Response C0 addresses the radio rotates through (matched to Thetis parsing).
const RESPONSE_ADDRS: [u8; 4] = [0x00, 0x08, 0x10, 0x18];

// ---------------------------------------------------------------------------
// Signal generator config (shared read-only, used to create per-client instances)
// ---------------------------------------------------------------------------

/// Configuration for creating per-client SignalGenerator instances.
#[derive(Clone, Copy)]
pub struct SiggenConfig {
    pub sample_rate: u32,
    pub noise_level: f64,
}

// ---------------------------------------------------------------------------
// Per-client state (owned by client task)
// ---------------------------------------------------------------------------

/// Internal implementation detail of the HPSDR Protocol 1 emulator.
/// Not a stable public API — fields may be reorganised at any time.
#[doc(hidden)]
pub struct ClientState {
    pub addr: SocketAddr,
    pub sample_rate: u32,
    pub nddc: u8,
    pub rx_frequencies: [u32; 12],
    pub tx_frequency: u32,
    /// PTT state. When toggled, `recording_freq` must be updated atomically
    /// (see `recording_freq` invariant).
    pub ptt: bool,
    pub tx_drive: u8,
    pub oc_outputs: u8,
    pub pa_enabled: bool,
    pub data_seq: u32,
    pub control_idx: u8,
    /// Per-client signal generator (owns its own phase accumulators).
    pub siggen: SignalGenerator,
    /// Per-client echo playback state.
    pub echo_playback: EchoPlaybackState,
    /// Pre-allocated packet buffer.
    pub pkt_buf: Vec<u8>,
    /// Pre-allocated frequency scratch buffer.
    pub freq_buf: [u32; 12],
    /// Pre-allocated per-DDC sample buffers.
    pub ddc_sample_bufs: Vec<Vec<Complex<f64>>>,
    /// Pre-allocated TX IQ unpack buffer.
    pub tx_iq_buf: Vec<Complex<f64>>,
    /// TX frequency locked at PTT-on for echo buffer tracking.
    ///
    /// # Invariant
    /// Must be set to `Some(tx_frequency)` when PTT turns on and cleared via
    /// `take()` when PTT turns off, in the same code path that calls
    /// `EchoBuffer::start_recording` / `stop_recording`. Never set independently.
    pub recording_freq: Option<u32>,
    /// Smoothed TX IQ peak envelope (0.0-1.0) for power meter simulation.
    pub tx_envelope: f64,
}

impl ClientState {
    pub fn new(addr: SocketAddr, _hw: &HwInfo, sg_cfg: &SiggenConfig) -> Self {
        Self {
            addr,
            sample_rate: sg_cfg.sample_rate,
            // TODO: use hw.hw.max_ddcs() once we support multiple DDCs in our clients
            nddc: 1,
            rx_frequencies: [7_074_000; 12],
            tx_frequency: 7_074_000,
            ptt: false,
            tx_drive: 0,
            oc_outputs: 0,
            pa_enabled: false,
            data_seq: 0,
            control_idx: 0,
            siggen: SignalGenerator::new(sg_cfg.sample_rate, sg_cfg.noise_level),
            echo_playback: EchoPlaybackState::new(),
            pkt_buf: vec![0u8; PACKET_SIZE],
            freq_buf: [0u32; 12],
            ddc_sample_bufs: (0..12).map(|_| Vec::with_capacity(128)).collect(),
            tx_iq_buf: vec![Complex::new(0.0, 0.0); MAX_TX_IQ_SAMPLES],
            recording_freq: None,
            tx_envelope: 0.0,
        }
    }

    pub fn next_seq(&mut self) -> u32 {
        let ret = self.data_seq;
        self.data_seq = self.data_seq.wrapping_add(1);
        ret
    }
}

// ---------------------------------------------------------------------------
// Client handle (tracked by main loop)
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
struct ClientHandle {
    cmd_tx: mpsc::Sender<Box<[u8]>>,
    stop_tx: Option<oneshot::Sender<()>>,
}

// ---------------------------------------------------------------------------
// Discovery response (pre-built once, reused for all requests)
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
fn build_discovery_response(hw: &HwInfo, nddc: u8) -> [u8; 60] {
    let mut buf = [0u8; 60];
    buf[0] = 0xEF;
    buf[1] = 0xFE;
    buf[2] = 0x02;
    buf[3..9].copy_from_slice(&hw.mac);
    buf[9] = hw.firmware_version;
    buf[10] = hw.hw.p1_code();
    buf[11] = 0; // protocol version 0 for P1
    buf[14] = hw.mercury_versions[0];
    buf[15] = hw.mercury_versions[1];
    buf[16] = hw.mercury_versions[2];
    buf[17] = hw.mercury_versions[3];
    buf[18] = hw.penny_version;
    buf[19] = hw.metis_version;
    buf[20] = nddc;
    buf
}

// ---------------------------------------------------------------------------
// Control processing (per-client state only, no lock)
// ---------------------------------------------------------------------------

/// Process a control sub-frame. Updates per-client state and returns whether
/// PTT changed (for the caller to handle echo lock if needed).
pub fn process_control(cs: &mut ClientState, c0: u8, c1: u8, c2: u8, c3: u8, c4: u8) -> bool {
    let mox = (c0 & 0x01) != 0;
    let addr_field = c0 & 0xFE;

    let ptt_changed = mox != cs.ptt;
    if ptt_changed {
        log::info!("P1 [{}] MOX -> {}", cs.addr, mox);
        cs.ptt = mox;
    }

    match addr_field {
        0x00 => {
            let rate_code = c1 & 0x03;
            if let Some(rate) = code_to_sample_rate(rate_code) {
                if cs.sample_rate != rate {
                    log::info!("P1 [{}] Sample rate -> {} Hz", cs.addr, rate);
                    cs.sample_rate = rate;
                    cs.siggen.sample_rate = rate;
                }
            }
            let nddc = ((c4 >> 3) & 0x07) + 1;
            if nddc != cs.nddc {
                log::info!("P1 [{}] Active DDCs -> {}", cs.addr, nddc);
                cs.nddc = nddc;
            }
            // OC outputs in C2[7:1]
            let oc_outputs = c2 >> 1;
            if oc_outputs != cs.oc_outputs {
                log::debug!("P1 [{}] OC outputs -> 0x{:02X}", cs.addr, oc_outputs);
                cs.oc_outputs = oc_outputs;
            }
        }
        0x02 => {
            let freq = u32::from_be_bytes([c1, c2, c3, c4]);
            if cs.tx_frequency != freq {
                log::info!("P1 [{}] TX freq -> {} Hz", cs.addr, freq);
                cs.tx_frequency = freq;
            }
        }
        a if (0x04..0x12).contains(&a) && (a % 2 == 0) => {
            let ddc_idx = ((a - 0x04) / 2) as usize;
            let freq = u32::from_be_bytes([c1, c2, c3, c4]);
            if ddc_idx < cs.rx_frequencies.len() && cs.rx_frequencies[ddc_idx] != freq {
                log::info!("P1 [{}] RX{} freq -> {} Hz", cs.addr, ddc_idx, freq);
                cs.rx_frequencies[ddc_idx] = freq;
            }
        }
        0x12 => {
            if cs.tx_drive != c1 {
                log::info!("P1 [{}] TX drive -> {}", cs.addr, c1);
                cs.tx_drive = c1;
            }
            // PA enable in C2 bit 3 (HL2)
            let pa_enable = (c2 & 0x08) != 0;
            if pa_enable != cs.pa_enabled {
                log::info!("P1 [{}] PA enable -> {}", cs.addr, pa_enable);
                cs.pa_enabled = pa_enable;
            }
        }
        _ => {}
    }

    ptt_changed
}

// ---------------------------------------------------------------------------
// Host data handling
// ---------------------------------------------------------------------------

/// Process a complete Protocol 1 host data packet synchronously.
///
/// Parses both sub-frames, updates `ClientState`, and mutates the echo buffer.
/// This is the sync core shared by [`handle_host_data`] and
/// `EmbeddedTransport::send()` in the web crate.
///
/// No-ops if `data` is shorter than `PACKET_SIZE` or lacks the P1 header.
pub fn process_host_frame(cs: &mut ClientState, data: &[u8], mut echo: Option<&mut EchoBuffer>) {
    if data.len() < PACKET_SIZE {
        return;
    }
    if data[0] != 0xEF || data[1] != 0xFE || data[2] != 0x01 {
        return;
    }
    for &offset in &[8usize, 520usize] {
        let sf = &data[offset..offset + SUBFRAME_SIZE];
        if sf[0..3] != SYNC {
            continue;
        }
        let ptt_changed = process_control(cs, sf[3], sf[4], sf[5], sf[6], sf[7]);

        // Track TX envelope for power meter simulation (mirrors handle_host_data).
        if cs.ptt {
            let tx_data = &sf[8..8 + MAX_TX_IQ_SAMPLES * 8];
            let n = unpack_tx_iq_16bit_into(tx_data, &mut cs.tx_iq_buf);
            let peak = cs.tx_iq_buf[..n]
                .iter()
                .map(|s| (s.re * s.re + s.im * s.im).sqrt())
                .fold(0.0f64, f64::max);
            if peak > cs.tx_envelope {
                cs.tx_envelope = peak;
            } else {
                cs.tx_envelope = cs.tx_envelope * 0.95 + peak * 0.05;
            }
            if let Some(echo_buf) = echo.as_deref_mut() {
                if ptt_changed {
                    cs.recording_freq = Some(cs.tx_frequency);
                    echo_buf.start_recording(cs.tx_frequency, cs.addr, cs.sample_rate);
                }
                if let Some(freq) = cs.recording_freq {
                    // Feed the full subframe verbatim — including all-zero
                    // packets. Those represent genuine silence from the host
                    // DSP (e.g. inter-element gaps in CW), NOT unused slots;
                    // dropping them collapses dit/dah spacing into a solid
                    // tone on echo playback. The web client pads short
                    // packets with `last_tx_iq`, not zeros, so preserving
                    // literal zeros no longer risks phase discontinuities.
                    echo_buf.feed(freq, cs.addr, &cs.tx_iq_buf[..n]);
                }
            }
        } else {
            cs.tx_envelope = 0.0;
            if let Some(echo_buf) = echo.as_deref_mut() {
                if ptt_changed {
                    if let Some(freq) = cs.recording_freq.take() {
                        echo_buf.stop_recording(freq, cs.addr);
                    }
                }
            }
        }
    }
}

pub async fn handle_host_data(
    cs: &mut ClientState,
    echo: &RwLock<Option<EchoBuffer>>,
    data: &[u8],
) {
    if data.len() < PACKET_SIZE {
        return;
    }

    for &offset in &[8usize, 520usize] {
        let sf = &data[offset..offset + SUBFRAME_SIZE];
        if sf[0..3] != SYNC {
            continue;
        }
        let (c0, c1, c2, c3, c4) = (sf[3], sf[4], sf[5], sf[6], sf[7]);
        let ptt_changed = process_control(cs, c0, c1, c2, c3, c4);

        // Unpack TX IQ data outside the write lock to reduce contention.
        // We always unpack when PTT is active — including zero data — so
        // the echo buffer records the inter-element gaps in CW mode.
        let tx_samples = if cs.ptt {
            let tx_data = &sf[8..8 + MAX_TX_IQ_SAMPLES * 8];
            let n = unpack_tx_iq_16bit_into(tx_data, &mut cs.tx_iq_buf);
            // Track peak envelope for power meter simulation
            let peak = cs.tx_iq_buf[..n]
                .iter()
                .map(|s| (s.re * s.re + s.im * s.im).sqrt())
                .fold(0.0f64, f64::max);
            // Fast attack, slow decay smoothing
            if peak > cs.tx_envelope {
                cs.tx_envelope = peak;
            } else {
                cs.tx_envelope = cs.tx_envelope * 0.95 + peak * 0.05;
            }
            Some(n)
        } else {
            cs.tx_envelope = 0.0;
            None
        };

        // Acquire write lock only for the actual echo buffer mutations:
        // PTT transition (start/stop recording) or feeding unpacked TX IQ data.
        if ptt_changed || tx_samples.is_some() {
            let mut echo_guard = echo.write().await;
            if let Some(echo_buf) = echo_guard.as_mut() {
                if ptt_changed {
                    if cs.ptt {
                        // Lock TX frequency at PTT-on so mid-PTT freq changes
                        // don't leak recorder refcounts.
                        cs.recording_freq = Some(cs.tx_frequency);
                        echo_buf.start_recording(cs.tx_frequency, cs.addr, cs.sample_rate);
                    } else if let Some(freq) = cs.recording_freq.take() {
                        echo_buf.stop_recording(freq, cs.addr);
                    }
                }
                if let Some(n) = tx_samples {
                    if let Some(freq) = cs.recording_freq {
                        // See process_host_frame: feed the full subframe so
                        // CW inter-element silence is preserved in the echo.
                        echo_buf.feed(freq, cs.addr, &cs.tx_iq_buf[..n]);
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Sub-frame building
// ---------------------------------------------------------------------------

/// Sync version of sub-frame building.
pub fn fill_subframe_sync(
    cs: &mut ClientState,
    hw: &HwInfo,
    echo: Option<&EchoBuffer>,
    buf: &mut [u8],
    offset: usize,
) {
    let nddc = cs.nddc.max(1) as usize;
    let spr = 504 / (6 * nddc + 2);

    // Sync
    buf[offset] = 0x7F;
    buf[offset + 1] = 0x7F;
    buf[offset + 2] = 0x7F;

    // Control response (C0-C4)
    let c0_addr = RESPONSE_ADDRS[cs.control_idx as usize % RESPONSE_ADDRS.len()];
    cs.control_idx = (cs.control_idx + 1) % RESPONSE_ADDRS.len() as u8;

    let ptt = cs.ptt;
    let tx_drive = cs.tx_drive;

    let ptt_bit = if ptt { 1u8 } else { 0u8 };
    buf[offset + 3] = c0_addr | ptt_bit;

    match c0_addr {
        0x00 => {
            buf[offset + 4] = 0x00;
            buf[offset + 5] = hw.firmware_version;
            buf[offset + 6] = hw.penny_version;
            buf[offset + 7] = 0x00;
        }
        0x08 => {
            let (exc, fwd) = if ptt && tx_drive > 0 {
                let drive_frac = tx_drive as f64 / 255.0;
                let envelope = cs.tx_envelope.clamp(0.0, 1.0);
                let power_frac = drive_frac * envelope;
                let fwd = (power_frac.sqrt() * 3800.0) as u16;
                let exc = (power_frac.sqrt() * 800.0) as u16;
                (exc, fwd)
            } else {
                (0, 0)
            };
            buf[offset + 4..offset + 6].copy_from_slice(&exc.to_be_bytes());
            buf[offset + 6..offset + 8].copy_from_slice(&fwd.to_be_bytes());
        }
        0x10 => {
            let rev = if ptt && tx_drive > 0 {
                let drive_frac = tx_drive as f64 / 255.0;
                let envelope = cs.tx_envelope.clamp(0.0, 1.0);
                let power_frac = drive_frac * envelope;
                let fwd = (power_frac.sqrt() * 3800.0) as u16;
                (fwd as f64 * 0.13).max(1.0) as u16
            } else {
                0
            };
            let supply: u16 = 3200;
            buf[offset + 4..offset + 6].copy_from_slice(&rev.to_be_bytes());
            buf[offset + 6..offset + 8].copy_from_slice(&supply.to_be_bytes());
        }
        0x18 => {
            let pa_amps: u16 = if ptt { tx_drive as u16 * 5 } else { 0 };
            let supply: u16 = 3200;
            buf[offset + 4..offset + 6].copy_from_slice(&pa_amps.to_be_bytes());
            buf[offset + 6..offset + 8].copy_from_slice(&supply.to_be_bytes());
        }
        _ => {
            buf[offset + 4..offset + 8].fill(0);
        }
    }

    // Copy frequencies to stack buffer
    cs.freq_buf[..nddc].copy_from_slice(&cs.rx_frequencies[..nddc]);
    let sample_rate = cs.sample_rate;

    // Generate IQ samples directly into pre-allocated per-DDC buffers.
    // SignalGenerator: per-client, no lock needed.
    if let Some(echo_ref) = echo {
        let echo_playback = &mut cs.echo_playback;
        let ddc_bufs = &mut cs.ddc_sample_bufs;
        let freq_buf = &cs.freq_buf;
        let siggen = &mut cs.siggen;

        for ddc_idx in 0..nddc {
            let ddc_buf = &mut ddc_bufs[ddc_idx];
            ddc_buf.resize(spr, Complex::new(0.0, 0.0));
            if echo_ref.has_data() {
                match echo_ref.mode() {
                    EchoMode::Live => echo_ref.generate_live_echo_into(
                        echo_playback,
                        &mut ddc_buf[..spr],
                        freq_buf[ddc_idx],
                        sample_rate,
                    ),
                    EchoMode::Loop => echo_ref.generate_echo_into(
                        echo_playback,
                        &mut ddc_buf[..spr],
                        freq_buf[ddc_idx],
                        sample_rate,
                    ),
                };
                siggen.add_noise_into(&mut ddc_buf[..spr]);
            } else {
                ddc_buf[..spr].fill(Complex::new(0.0, 0.0));
                siggen.add_noise_into(&mut ddc_buf[..spr]);
            }
        }
    } else {
        let siggen = &mut cs.siggen;
        let ddc_bufs = &mut cs.ddc_sample_bufs;

        for ddc_buf in ddc_bufs.iter_mut().take(nddc) {
            ddc_buf.resize(spr, Complex::new(0.0, 0.0));
            ddc_buf[..spr].fill(Complex::new(0.0, 0.0));
            siggen.add_noise_into(&mut ddc_buf[..spr]);
        }
    }

    // Pack interleaved: [I(3B) Q(3B)] x nddc + [Mic(2B)] per sample row
    let mut data_offset = offset + 8;
    for row in 0..spr {
        for ddc_idx in 0..nddc {
            data_offset = pack_iq_24bit_into(buf, data_offset, cs.ddc_sample_bufs[ddc_idx][row]);
        }
        buf[data_offset] = 0;
        buf[data_offset + 1] = 0;
        data_offset += 2;
    }

    // Zero any unused padding bytes at end of subframe (protocol requires zero-fill).
    if data_offset < offset + SUBFRAME_SIZE {
        buf[data_offset..offset + SUBFRAME_SIZE].fill(0);
    }
}

pub fn build_data_packet_sync(cs: &mut ClientState, hw: &HwInfo, echo: Option<&EchoBuffer>) {
    let seq = cs.next_seq();

    // Header
    cs.pkt_buf[0] = 0xEF;
    cs.pkt_buf[1] = 0xFE;
    cs.pkt_buf[2] = 0x01; // data packet
    cs.pkt_buf[3] = 0x06; // endpoint 6
    cs.pkt_buf[4..8].copy_from_slice(&seq.to_be_bytes());

    // Take buffer out to avoid borrow conflict with fill_subframe
    let mut buf = std::mem::take(&mut cs.pkt_buf);
    fill_subframe_sync(cs, hw, echo, &mut buf, 8);
    fill_subframe_sync(cs, hw, echo, &mut buf, 520);
    cs.pkt_buf = buf;
}

// ---------------------------------------------------------------------------
// Data packet building
// ---------------------------------------------------------------------------

pub async fn build_data_packet(
    cs: &mut ClientState,
    hw: &HwInfo,
    echo: &RwLock<Option<EchoBuffer>>,
) {
    // No full-buffer zeroing — fill_subframe writes all meaningful bytes
    // and zeroes only the unused tail of each subframe.

    let seq = cs.next_seq();

    // Header
    cs.pkt_buf[0] = 0xEF;
    cs.pkt_buf[1] = 0xFE;
    cs.pkt_buf[2] = 0x01; // data packet
    cs.pkt_buf[3] = 0x06; // endpoint 6
    cs.pkt_buf[4..8].copy_from_slice(&seq.to_be_bytes());

    // Take buffer out to avoid borrow conflict with fill_subframe
    let mut buf = std::mem::take(&mut cs.pkt_buf);
    {
        let echo_guard = echo.read().await;
        fill_subframe_sync(cs, hw, echo_guard.as_ref(), &mut buf, 8);
        fill_subframe_sync(cs, hw, echo_guard.as_ref(), &mut buf, 520);
    }
    cs.pkt_buf = buf;
}

// ---------------------------------------------------------------------------
// Per-client task
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
async fn client_task(
    addr: SocketAddr,
    socket: Arc<UdpSocket>,
    hw: Arc<HwInfo>,
    sg_cfg_shared: Arc<RwLock<SiggenConfig>>,
    echo: Arc<RwLock<Option<EchoBuffer>>>,
    mut cmd_rx: mpsc::Receiver<Box<[u8]>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut cs = {
        let cfg = sg_cfg_shared.read().await;
        ClientState::new(addr, &hw, &cfg)
    };

    let nddc = cs.nddc.max(1) as usize;
    let spr = 504 / (6 * nddc + 2);
    let samples_per_packet = spr * 2;
    let interval = Duration::from_secs_f64(samples_per_packet as f64 / cs.sample_rate as f64);

    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    let mut cur_nddc = nddc;
    let mut cur_sample_rate = cs.sample_rate;

    loop {
        tokio::select! {
            _ = timer.tick() => {
                // Update noise level from shared config
                {
                    let cfg = sg_cfg_shared.read().await;
                    cs.siggen.noise_level = cfg.noise_level;
                }

                // Recreate timer if sample rate or NDC count changed
                let new_nddc = cs.nddc.max(1) as usize;
                let new_rate = cs.sample_rate;
                if new_nddc != cur_nddc || new_rate != cur_sample_rate {
                    log::info!(
                        "P1 [{}] Streaming params changed: nddc {} -> {}, rate {} -> {}",
                        addr, cur_nddc, new_nddc, cur_sample_rate, new_rate
                    );
                    cur_nddc = new_nddc;
                    cur_sample_rate = new_rate;
                    let new_spr = 504 / (6 * cur_nddc + 2);
                    let new_spp = new_spr * 2;
                    let new_interval = Duration::from_secs_f64(
                        new_spp as f64 / cur_sample_rate as f64
                    );
                    timer = tokio::time::interval(new_interval);
                    timer.set_missed_tick_behavior(
                        tokio::time::MissedTickBehavior::Burst
                    );
                }

                build_data_packet(&mut cs, &hw, &echo).await;
                if let Err(e) = socket.send_to(&cs.pkt_buf, addr).await {
                    log::error!("P1 [{}] Send error: {}", addr, e);
                }
            }
            Some(data) = cmd_rx.recv() => {
                handle_host_data(&mut cs, &echo, &data).await;
            }
            _ = &mut stop_rx => {
                log::info!("P1 [{}] Client task stopping", addr);
                break;
            }
        }
    }

    // Clean up: if this client was transmitting, release the recorder refcount
    // so other clients aren't left with a stale active-count.
    if let Some(freq) = cs.recording_freq.take() {
        let mut echo_guard = echo.write().await;
        if let Some(echo_buf) = echo_guard.as_mut() {
            echo_buf.stop_recording(freq, cs.addr);
            log::info!("P1 [{}] Released echo recorder on {} Hz", addr, freq);
        }
    }
}

// ---------------------------------------------------------------------------
// Main entry point
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub async fn run_protocol1(
    hw: Arc<HwInfo>,
    sg_cfg: Arc<RwLock<SiggenConfig>>,
    echo: Arc<RwLock<Option<EchoBuffer>>>,
    bind_addr: &str,
    max_clients: usize,
) -> std::io::Result<()> {
    let bind = format!("{}:{}", bind_addr, PORT);
    let socket = UdpSocket::bind(&bind).await.map_err(|e| {
        log::error!(
            "Failed to bind UDP {} ({}). On Linux, ports < 1024 require root or CAP_NET_BIND_SERVICE.",
            bind, e
        );
        e
    })?;

    // Enable broadcast so the socket can receive discovery broadcasts
    // even when bound to a specific interface address.
    if let Err(e) = socket.set_broadcast(true) {
        log::warn!("Failed to enable SO_BROADCAST: {}", e);
    }

    log::info!("Protocol 1 listening on UDP {}", bind);
    log::info!(
        "Radio: {} (code={}, DDCs={})",
        hw.hw,
        hw.hw.p1_code(),
        hw.hw.max_ddcs()
    );
    log::info!("MAC: {}", hw.mac_string());

    run_protocol1_on(socket, hw, sg_cfg, echo, max_clients).await;
    Ok(())
}

/// Run Protocol 1 on a pre-bound socket.
/// Useful for tests that need to bind to a random port.
#[cfg(not(target_arch = "wasm32"))]
pub async fn run_protocol1_on(
    socket: UdpSocket,
    hw: Arc<HwInfo>,
    sg_cfg: Arc<RwLock<SiggenConfig>>,
    echo: Arc<RwLock<Option<EchoBuffer>>>,
    max_clients: usize,
) {
    let socket = Arc::new(socket);
    let mut recv_buf = vec![0u8; 2048];
    let mut clients: HashMap<SocketAddr, ClientHandle> = HashMap::new();

    // Pre-build discovery response (HwInfo is immutable).
    let discovery_resp = build_discovery_response(&hw, hw.hw.max_ddcs());

    // Counter for periodic dead-client cleanup instead of per-packet retain.
    let mut cleanup_counter: u32 = 0;

    loop {
        match socket.recv_from(&mut recv_buf).await {
            Ok((len, addr)) => {
                let data = &recv_buf[..len];
                log::debug!("P1 Recv {} bytes from {} type=0x{:02X}", len, addr, data[2]);
                if len < 4 || data[0] != 0xEF || data[1] != 0xFE {
                    continue;
                }
                match data[2] {
                    0x02 => {
                        // Discovery — stateless, respond with pre-built packet
                        log::info!("P1 Discovery request from {}", addr);
                        let _ = socket.send_to(&discovery_resp, addr).await;
                        log::info!(
                            "P1 Discovery response sent ({} bytes)",
                            discovery_resp.len()
                        );
                    }
                    0x04 if len > 3 => {
                        if data[3] == 0x01 {
                            // Start streaming
                            if clients.contains_key(&addr) {
                                log::info!("P1 [{}] Already connected, ignoring start", addr);
                                continue;
                            }
                            if clients.len() >= max_clients {
                                log::warn!(
                                    "P1 [{}] Rejected: client limit reached ({max_clients})",
                                    addr
                                );
                                continue;
                            }
                            let (cmd_tx, cmd_rx) = mpsc::channel(64);
                            let (stop_tx, stop_rx) = oneshot::channel();
                            tokio::spawn(client_task(
                                addr,
                                Arc::clone(&socket),
                                Arc::clone(&hw),
                                Arc::clone(&sg_cfg),
                                Arc::clone(&echo),
                                cmd_rx,
                                stop_rx,
                            ));
                            clients.insert(
                                addr,
                                ClientHandle {
                                    cmd_tx,
                                    stop_tx: Some(stop_tx),
                                },
                            );
                            log::info!("P1 [{}] Client connected ({} total)", addr, clients.len());
                        } else if data[3] == 0x00 {
                            // Stop streaming
                            if let Some(mut handle) = clients.remove(&addr) {
                                if let Some(stop_tx) = handle.stop_tx.take() {
                                    let _ = stop_tx.send(());
                                }
                                log::info!(
                                    "P1 [{}] Client disconnected ({} remain)",
                                    addr,
                                    clients.len()
                                );
                            } else {
                                log::info!("P1 [{}] Stop (not connected)", addr);
                            }
                        }
                    }
                    0x01 => {
                        // Data packet — forward to client task
                        if let Some(handle) = clients.get(&addr) {
                            if handle.cmd_tx.try_send(Box::from(data)).is_err() {
                                log::warn!("P1 [{}] Command channel full, dropping packet", addr);
                            }
                        } else {
                            log::debug!("P1 [{}] Pre-start data packet (no client yet)", addr);
                        }
                        // Periodic dead-client cleanup (every 100 data packets
                        // instead of scanning all clients on every packet).
                        cleanup_counter += 1;
                        if cleanup_counter >= 100 {
                            cleanup_counter = 0;
                            clients.retain(|a, handle| {
                                if handle.cmd_tx.is_closed() {
                                    log::info!("P1 [{}] Client task exited, removing", a);
                                    false
                                } else {
                                    true
                                }
                            });
                        }
                    }
                    _ => {}
                }
            }
            Err(e) => {
                log::error!("P1 Recv error: {}", e);
            }
        }
    }
}
