use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use num_complex::Complex;
use ringbuf::traits::{Consumer, Observer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tokio::sync::mpsc;

use crate::discovery;
use crate::transport::UdpTransport;
use crate::types::*;

const PORT: u16 = 1024;
const PACKET_SIZE: usize = 1032;
const SUBFRAME_SIZE: usize = 512;
const SYNC: [u8; 3] = [0x7F, 0x7F, 0x7F];

/// If no packet is received within this duration, the connection is considered lost.
const RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// Commands that can be sent to the Protocol 1 client while it is running.
#[derive(Debug)]
pub enum P1Command {
    SetRxFrequency { ddc: usize, freq_hz: u32 },
    SetTxFrequency(u32),
    SetSampleRate(u32),
    SetNddc(u8),
    SetTxDrive(u8),
    SetPtt(bool),
    SetCwMode(bool),
    SetRxAttenuation(u8),
    Stop,
}

/// Protocol 1 client for connecting to HPSDR hardware.
pub struct Protocol1Client {
    transport: UdpTransport,
    device: DiscoveredDevice,
    running: bool,
    // Configuration
    sample_rate: u32,
    nddc: u8,
    rx_frequencies: [u32; 12],
    tx_frequency: u32,
    tx_drive: u8,
    ptt: bool,
    rx_attenuation: u8,
    // Sequence tracking
    tx_seq: u32,
    rx_seq: u32,
    // Control command state: which address to send next
    control_idx: u8,
    // Channels
    iq_senders: HashMap<usize, mpsc::Sender<Vec<Complex<f64>>>>,
    tx_iq_consumer: Option<HeapCons<Complex<f64>>>,
    status_sender: mpsc::Sender<RadioStatus>,
    status_receiver: Option<mpsc::Receiver<RadioStatus>>,
    // Command channel for runtime control
    cmd_sender: mpsc::Sender<P1Command>,
    cmd_receiver: Option<mpsc::Receiver<P1Command>>,
    // CW mode flag: when true, mic L/R channels are zeroed (tone is in IQ only)
    tx_cw_mode: bool,
    // TX IQ buffer: accumulates DSP blocks, drains 63 per subframe
    tx_iq_buffer: Vec<Complex<f64>>,
    // Pre-allocated packet buffer for the streaming hot path
    host_pkt_buf: Vec<u8>,
    // Pre-allocated packet buffer for send_control
    ctrl_pkt_buf: Vec<u8>,
    // Pre-allocated per-DDC sample buffers for parse_radio_subframe
    ddc_sample_bufs: Vec<Vec<Complex<f64>>>,
    // Accumulated status across response address rotation. Each subframe
    // carries the data for ONE response-address slot (0x00 / 0x08 / 0x10
    // / 0x18 in HPSDR P1), so a single subframe never has both fwd and
    // rev fresh. We accumulate values into `accumulated_status` across
    // the rotation and emit only at the cycle boundary — see
    // `last_status_addr` below.
    accumulated_status: RadioStatus,
    // Last response-address byte we saw, used to detect cycle wrap.
    // When the rotation comes back round (current addr <= previous),
    // the just-completed cycle has fully populated fwd / rev / supply /
    // pa_current, so we emit ONE coherent status. Without this every
    // subframe emitted its own status which mixed fresh fields with
    // stale ones from prior cycles — broke SWR (computed from a
    // mismatched fwd/rev pair) and made the rev-power readout look
    // stuck near zero.
    last_status_addr: Option<u8>,
    // TX debug counters
    tx_subframes_with_data: u64,
    tx_subframes_empty: u64,
    /// Voice-TX underrun guard: increments each time the adaptive pacing
    /// asked us to send a packet but the IQ buffer didn't have a full
    /// `samples_per_packet` worth of samples yet. Counts skipped *send*
    /// attempts (one per guard-firing), not subframes — so it's
    /// directly comparable to packets-per-second figures rather than
    /// to `sf_data` / `sf_empty`. Should be ~0 if the wire-pack and
    /// the DSP producer are well-matched; large values mean burst-mode
    /// pacing is outrunning DSP and the guard is doing its job.
    tx_underrun_skips: u64,
    tx_debug_log_counter: u32,
    // FIFO-estimate TX pacing (voice only): accumulates on each packet sent
    // and drains at `sample_rate`. Lets us burst packets at PTT start to
    // build a radio-side jitter cushion, then throttle back to wire rate.
    tx_fifo_estimate: f64,
    tx_last_send: Option<tokio::time::Instant>,
    tx_fifo_log_counter: u32,
    // Wire-pack clipping diagnostics (per-PTT-session counters).
    tx_samples_total: u64,
    tx_samples_clipped_re: u64,
    tx_samples_clipped_im: u64,
    tx_max_excess: f64,
}

impl Protocol1Client {
    /// Discover all HPSDR devices on the network using Protocol 1.
    pub async fn discover(timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        discovery::discover_on_interfaces(
            build_p1_discovery_request,
            parse_p1_discovery_response,
            PORT,
            timeout,
        )
        .await
    }

    /// Send a discovery request to a specific address (unicast).
    pub async fn discover_at(addr: Ipv4Addr, timeout: Duration) -> Result<Vec<DiscoveredDevice>> {
        discovery::discover_at_addr(
            build_p1_discovery_request,
            parse_p1_discovery_response,
            addr,
            PORT,
            timeout,
        )
        .await
    }

    /// Connect to a discovered Protocol 1 device.
    pub async fn connect(device: &DiscoveredDevice) -> Result<Self> {
        let transport = UdpTransport::bind_any().await?;
        let (status_tx, status_rx) = mpsc::channel(64);
        let (cmd_tx, cmd_rx) = mpsc::channel(64);

        Ok(Self {
            transport,
            device: device.clone(),
            running: false,
            sample_rate: 48_000,
            nddc: 1,
            rx_frequencies: [7_074_000; 12],
            tx_frequency: 7_074_000,
            tx_drive: 128,
            ptt: false,
            rx_attenuation: 0,
            tx_seq: 0,
            rx_seq: 0,
            control_idx: 0,
            iq_senders: HashMap::new(),
            tx_iq_consumer: None,
            tx_cw_mode: false,
            tx_iq_buffer: Vec::new(),
            status_sender: status_tx,
            status_receiver: Some(status_rx),
            cmd_sender: cmd_tx,
            cmd_receiver: Some(cmd_rx),
            host_pkt_buf: vec![0u8; PACKET_SIZE],
            ctrl_pkt_buf: vec![0u8; PACKET_SIZE],
            ddc_sample_bufs: Vec::new(),
            accumulated_status: RadioStatus::default(),
            last_status_addr: None,
            tx_subframes_with_data: 0,
            tx_subframes_empty: 0,
            tx_underrun_skips: 0,
            tx_debug_log_counter: 0,
            tx_fifo_estimate: 0.0,
            tx_last_send: None,
            tx_fifo_log_counter: 0,
            tx_samples_total: 0,
            tx_samples_clipped_re: 0,
            tx_samples_clipped_im: 0,
            tx_max_excess: 0.0,
        })
    }

    /// Start streaming IQ data from the radio.
    pub async fn start(&mut self) -> Result<()> {
        // Send start command
        let mut pkt = vec![0u8; 64];
        pkt[0] = 0xEF;
        pkt[1] = 0xFE;
        pkt[2] = 0x04;
        pkt[3] = 0x01; // start
        self.transport.send_to(&pkt, self.device.addr).await?;
        self.running = true;
        log::debug!("P1 Start command sent to {}", self.device.addr);

        // Send initial configuration
        self.send_config().await?;
        Ok(())
    }

    /// Stop streaming.
    pub async fn stop(&mut self) -> Result<()> {
        let mut pkt = vec![0u8; 64];
        pkt[0] = 0xEF;
        pkt[1] = 0xFE;
        pkt[2] = 0x04;
        pkt[3] = 0x00; // stop
        self.transport.send_to(&pkt, self.device.addr).await?;
        self.running = false;
        log::debug!("P1 Stop command sent");
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

    pub fn set_nddc(&mut self, count: u8) {
        self.nddc = count.max(1);
    }

    pub fn tx_drive(&self) -> u8 {
        self.tx_drive
    }

    pub fn set_tx_drive(&mut self, drive: u8) {
        self.tx_drive = drive;
    }

    pub fn set_ptt(&mut self, on: bool) {
        self.ptt = on;
    }

    pub fn set_rx_attenuation(&mut self, db: u8) {
        self.rx_attenuation = db;
    }

    /// Get a sender for runtime commands. The run() loop polls this channel.
    pub fn command_sender(&self) -> mpsc::Sender<P1Command> {
        self.cmd_sender.clone()
    }

    /// Get a receiver for IQ data from a specific DDC.
    pub fn rx_iq_stream(&mut self, ddc: usize) -> mpsc::Receiver<Vec<Complex<f64>>> {
        if self.iq_senders.contains_key(&ddc) {
            log::warn!(
                "rx_iq_stream called again for DDC {ddc} — previous receiver will be orphaned"
            );
        }
        let (tx, rx) = mpsc::channel(256);
        self.iq_senders.insert(ddc, tx);
        rx
    }

    /// Get a producer for TX IQ data (lock-free ring buffer, zero-copy).
    pub fn tx_iq_sink(&mut self) -> HeapProd<Complex<f64>> {
        if self.tx_iq_consumer.is_some() {
            log::warn!("tx_iq_sink called again — previous consumer will be orphaned");
        }
        // 256 blocks * 1024 samples = 262144 — enough to absorb DSP jitter
        let rb = HeapRb::<Complex<f64>>::new(262_144);
        let (prod, cons) = rb.split();
        self.tx_iq_consumer = Some(cons);
        prod
    }

    /// Get a receiver for radio status updates.
    ///
    /// Returns `None` if already taken (can only be called once).
    pub fn status_stream(&mut self) -> Option<mpsc::Receiver<RadioStatus>> {
        self.status_receiver.take()
    }

    /// Run the main receive/transmit loop. This blocks until stop() is called
    /// or the connection is lost.
    pub async fn run(&mut self) -> Result<()> {
        let mut recv_buf = vec![0u8; 2048];
        let spr = self.samples_per_subframe();
        let samples_per_packet = (spr * 2) as f64;
        // Wire-rate interval: used for RX and CW TX (no cushion needed).
        let wire_rate_interval =
            Duration::from_secs_f64(samples_per_packet / self.sample_rate as f64);

        let mut cmd_rx = self.cmd_receiver.take();
        let mut last_recv = tokio::time::Instant::now();
        let mut next_tx_send = tokio::time::Instant::now();

        while self.running {
            tokio::select! {
                // Send control/TX data at an adaptive cadence (voice TX) or
                // wire rate (RX / CW TX).
                _ = tokio::time::sleep_until(next_tx_send) => {
                    let now = tokio::time::Instant::now();

                    // Voice-TX underrun guard: if we don't have a full packet's
                    // worth of IQ ready (in either the local refill buffer or
                    // the lock-free consumer), skip this send and retry in
                    // 500 µs so the wire-pack never emits a subframe padded
                    // with zeros.
                    //
                    // Why this matters: the adaptive pacing below can drop
                    // `next_delay` to 0 µs to burst-prime the radio FIFO at
                    // PTT-on. Once the burst started, it kept re-entering
                    // burst mode every time `tx_fifo_estimate` (a fictional
                    // model of the radio FIFO based on packet timing, not on
                    // actual buffer fill) dipped below `low_water`. The DSP
                    // produces in chunks (1024-sample blocks every ~21 ms)
                    // while burst mode pulls 126 samples per packet at
                    // ~3000 pkt/s, so the consumer regularly drained dry and
                    // `fill_host_subframe` packed 63 zero samples instead of
                    // bailing. Those zero subframes injected hard
                    // discontinuities into the on-air signal at a 19 % duty
                    // cycle (sf_empty / (sf_empty + sf_data) measured ~412 /
                    // 2150 in repro logs), creating broadband splatter that
                    // is invisible to the host's pre-wire spectrum probe but
                    // very visible to the receiver. CW TX is exempt — its
                    // modulator generates a continuous carrier and the pacing
                    // is wire-rate locked, so the underrun pattern doesn't
                    // exist there.
                    let voice_tx = self.ptt && !self.tx_cw_mode;
                    let pkt_iq_samples = samples_per_packet as usize;
                    let voice_iq_avail = self.tx_iq_buffer.len()
                        + self
                            .tx_iq_consumer
                            .as_ref()
                            .map(|c| c.occupied_len())
                            .unwrap_or(0);
                    let voice_underrun = voice_tx && voice_iq_avail < pkt_iq_samples;

                    let next_delay = if voice_underrun {
                        // Skip this send entirely: a packet whose IQ section
                        // is zero-padded would inject 63 silent samples in the
                        // middle of a continuous transmission and create
                        // broadband splatter. Re-arm in 500 µs and try again
                        // when DSP has produced more samples.
                        self.tx_underrun_skips = self.tx_underrun_skips.saturating_add(1);
                        Duration::from_micros(500)
                    } else {
                        // Update FIFO estimate: the radio drains at sample_rate.
                        if let Some(last) = self.tx_last_send {
                            let elapsed = now.duration_since(last).as_secs_f64();
                            self.tx_fifo_estimate -= elapsed * self.sample_rate as f64;
                            if self.tx_fifo_estimate < 0.0 {
                                self.tx_fifo_estimate = 0.0;
                            }
                        }
                        self.tx_last_send = Some(now);

                        self.send_host_data_packet().await?;
                        self.tx_fifo_estimate += samples_per_packet;

                        // Voice TX: burst at start to prime the radio-side
                        // FIFO, then throttle to keep ~30 ms of cushion
                        // against network jitter. CW and RX keep fixed
                        // wire-rate pacing.
                        if voice_tx {
                            let sr = self.sample_rate as f64;
                            let high_water = 0.030 * sr;
                            let low_water = 0.006 * sr;
                            if self.tx_fifo_estimate > high_water {
                                Duration::from_micros(2000)
                            } else if self.tx_fifo_estimate > low_water {
                                Duration::from_micros(500)
                            } else {
                                Duration::ZERO
                            }
                        } else {
                            wire_rate_interval
                        }
                    };
                    // Anchor the next-send schedule to the previous deadline
                    // when on-time, fall back to `now + next_delay` when the
                    // schedule has slipped into the past.
                    //
                    // Why anchor: every iteration's tokio wake + UDP-send
                    // latency (~1 ms on a busy localhost loop) baked into
                    // `now + next_delay` made the host's TX rate settle
                    // around 270 pkt/s instead of the 380 pkt/s wire rate.
                    // The emulator's RX timer is anchored (it uses
                    // `tokio::time::interval`), so a drifted host produced
                    // ~30 % fewer samples per wall-clock second than the
                    // listener's live-mode read consumed — those reads then
                    // fell onto the LIVE_DELAY safety margin and emitted
                    // 63-sample silent subframes, scrambling a CW tone over
                    // several kHz of bandwidth.
                    //
                    // Why fall back when slipping: if we anchor unconditionally
                    // and the schedule is e.g. 10 ms in the past, sleep_until
                    // returns immediately for several iterations in a row,
                    // bursting catch-up sends. Each catch-up send drains 63
                    // samples from the host's TX ring; if the producer was
                    // in the middle of a 2 ms gap when the burst hit, the
                    // ring underflows and the host pads the subframe with
                    // zeros (loop mode then captures internal zero runs that
                    // break up the recorded tone). Falling back to
                    // `now + next_delay` on slip preserves cadence anchoring
                    // for the steady-state case (the user's reported bug)
                    // while preventing the burst regression.
                    let scheduled = next_tx_send + next_delay;
                    next_tx_send = if scheduled >= now {
                        scheduled
                    } else {
                        now + next_delay
                    };

                    if voice_tx {
                        self.tx_fifo_log_counter += 1;
                        if self.tx_fifo_log_counter.is_multiple_of(400) {
                            log::debug!(
                                "[P1 TX PACE] fifo_est={:.0} samp ({:.1} ms), next_sleep={:?}",
                                self.tx_fifo_estimate,
                                self.tx_fifo_estimate * 1000.0 / self.sample_rate as f64,
                                next_delay
                            );
                        }
                    }
                }
                // Receive data from radio
                result = self.transport.recv_from(&mut recv_buf) => {
                    match result {
                        Ok((len, _addr)) => {
                            last_recv = tokio::time::Instant::now();
                            self.handle_radio_packet(&recv_buf[..len]);
                        }
                        Err(e) => {
                            log::error!("P1 Recv error: {}", e);
                        }
                    }
                }
                // Process runtime commands
                Some(cmd) = async {
                    match &mut cmd_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    self.handle_command(cmd).await?;
                }
                // Timeout detection
                _ = tokio::time::sleep_until(last_recv + RECV_TIMEOUT) => {
                    log::error!("P1 No data received for {:?}, connection lost", RECV_TIMEOUT);
                    return Err(ProtocolError::ConnectionLost);
                }
            }
        }
        Ok(())
    }

    async fn handle_command(&mut self, cmd: P1Command) -> Result<()> {
        match cmd {
            P1Command::SetRxFrequency { ddc, freq_hz } => {
                self.set_rx_frequency(ddc, freq_hz);
                log::debug!("P1 RX {} freq -> {} Hz", ddc, freq_hz);
            }
            P1Command::SetTxFrequency(freq) => {
                self.set_tx_frequency(freq);
                log::debug!("P1 TX freq -> {} Hz", freq);
            }
            P1Command::SetSampleRate(rate) => {
                self.set_sample_rate(rate);
                log::debug!("P1 Sample rate -> {}", rate);
            }
            P1Command::SetNddc(n) => {
                self.set_nddc(n);
                log::debug!("P1 NDDC -> {}", n);
            }
            P1Command::SetTxDrive(drive) => {
                self.set_tx_drive(drive);
                log::debug!("P1 TX drive -> {}", drive);
            }
            P1Command::SetPtt(on) => {
                let was = self.ptt;
                self.set_ptt(on);
                log::debug!(
                    "P1 PTT {} -> {} (drive={}, tx_iq_consumer={})",
                    if was { "ON" } else { "OFF" },
                    if on { "ON" } else { "OFF" },
                    self.tx_drive,
                    if self.tx_iq_consumer.is_some() {
                        "connected"
                    } else {
                        "NONE"
                    }
                );
                if on && !was {
                    // New TX session — start with empty radio FIFO estimate
                    // so the sender bursts until the cushion is built.
                    self.tx_fifo_estimate = 0.0;
                    self.tx_last_send = None;
                    self.tx_fifo_log_counter = 0;
                    self.tx_samples_total = 0;
                    self.tx_samples_clipped_re = 0;
                    self.tx_samples_clipped_im = 0;
                    self.tx_max_excess = 0.0;
                    self.tx_underrun_skips = 0;
                }
                if !on {
                    log::debug!(
                        "P1 PTT OFF stats: subframes_with_data={}, subframes_empty={}, underrun_skips={}, buf_remaining={}",
                        self.tx_subframes_with_data,
                        self.tx_subframes_empty,
                        self.tx_underrun_skips,
                        self.tx_iq_buffer.len()
                    );
                    let total = self.tx_samples_total.max(1);
                    let pct_re = self.tx_samples_clipped_re as f64 * 100.0 / total as f64;
                    let pct_im = self.tx_samples_clipped_im as f64 * 100.0 / total as f64;
                    log::info!(
                        "[P1 TX CLIP] session: total={} clipped_re={} ({:.3}%) clipped_im={} ({:.3}%) max_excess={:.4} (1.0 = wire-pack ceiling)",
                        self.tx_samples_total,
                        self.tx_samples_clipped_re,
                        pct_re,
                        self.tx_samples_clipped_im,
                        pct_im,
                        self.tx_max_excess,
                    );
                    self.tx_iq_buffer.clear();
                    self.tx_subframes_with_data = 0;
                    self.tx_subframes_empty = 0;
                    self.tx_underrun_skips = 0;
                    self.tx_debug_log_counter = 0;
                }
            }
            P1Command::SetCwMode(cw) => {
                self.tx_cw_mode = cw;
                log::debug!("P1 TX CW mode -> {}", cw);
            }
            P1Command::SetRxAttenuation(db) => {
                self.set_rx_attenuation(db);
                log::debug!("P1 RX attenuation -> {} dB", db);
            }
            P1Command::Stop => {
                self.stop().await?;
            }
        }
        Ok(())
    }

    // -- Internal methods -----------------------------------------------------

    fn samples_per_subframe(&self) -> usize {
        // Protocol 1 supports 1-8 DDCs; clamp to prevent division yielding 0
        // (which would cause a busy-loop timer at 0-duration interval).
        let nddc = (self.nddc.max(1) as usize).min(8);
        504 / (6 * nddc + 2)
    }

    /// Send initial configuration commands.
    async fn send_config(&mut self) -> Result<()> {
        // Send sample rate + nddc + duplex (address 0x00)
        let rate_code = sample_rate_to_p1_code(self.sample_rate);
        let nddc_bits = ((self.nddc.max(1) - 1) & 0x07) << 3;
        let duplex = 0x04;
        self.send_control(0x00, rate_code, 0, 0, nddc_bits | duplex)
            .await?;

        // Send TX frequency (address 0x02)
        let tx_bytes = self.tx_frequency.to_be_bytes();
        self.send_control(0x02, tx_bytes[0], tx_bytes[1], tx_bytes[2], tx_bytes[3])
            .await?;

        // Send RX frequencies
        for i in 0..self.nddc.min(8) as usize {
            let addr = 0x04 + (i as u8 * 2);
            let freq_bytes = self.rx_frequencies[i].to_be_bytes();
            self.send_control(
                addr,
                freq_bytes[0],
                freq_bytes[1],
                freq_bytes[2],
                freq_bytes[3],
            )
            .await?;
        }

        // Send TX drive (address 0x12) + PA enable for HL2
        let pa_c2 = match self.device.hw_type {
            HpsdrHw::HermesLite => 0x08,
            _ => 0x00,
        };
        self.send_control(0x12, self.tx_drive, pa_c2, 0, 0).await?;

        Ok(())
    }

    /// Send a single control command embedded in a host data packet.
    async fn send_control(&mut self, c0: u8, c1: u8, c2: u8, c3: u8, c4: u8) -> Result<()> {
        self.ctrl_pkt_buf.fill(0);
        self.ctrl_pkt_buf[0] = 0xEF;
        self.ctrl_pkt_buf[1] = 0xFE;
        self.ctrl_pkt_buf[2] = 0x01; // data
        self.ctrl_pkt_buf[3] = 0x02; // endpoint 2 (host to radio)
        let seq = self.tx_seq;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        self.ctrl_pkt_buf[4..8].copy_from_slice(&seq.to_be_bytes());

        // Sub-frame A: sync + control
        self.ctrl_pkt_buf[8..11].copy_from_slice(&SYNC);
        let mox_bit = if self.ptt { 0x01 } else { 0x00 };
        self.ctrl_pkt_buf[11] = c0 | mox_bit;
        self.ctrl_pkt_buf[12] = c1;
        self.ctrl_pkt_buf[13] = c2;
        self.ctrl_pkt_buf[14] = c3;
        self.ctrl_pkt_buf[15] = c4;
        // Rest of sub-frame A: TX IQ silence

        // Sub-frame B: sync + address 0x00 with current sample rate + NDC + duplex
        self.ctrl_pkt_buf[520..523].copy_from_slice(&SYNC);
        let nddc_bits_b = ((self.nddc.max(1) - 1) & 0x07) << 3;
        self.ctrl_pkt_buf[523] = mox_bit; // C0 = 0x00 | mox
        self.ctrl_pkt_buf[524] = sample_rate_to_p1_code(self.sample_rate); // C1
                                                                           // C2, C3 = 0 (already zeroed)
        self.ctrl_pkt_buf[527] = nddc_bits_b | 0x04; // C4: nddc + duplex

        self.transport
            .send_to(&self.ctrl_pkt_buf, self.device.addr)
            .await?;
        Ok(())
    }

    /// Build and send a host data packet with rotating control commands and TX IQ.
    async fn send_host_data_packet(&mut self) -> Result<()> {
        self.host_pkt_buf.fill(0);
        self.host_pkt_buf[0] = 0xEF;
        self.host_pkt_buf[1] = 0xFE;
        self.host_pkt_buf[2] = 0x01;
        self.host_pkt_buf[3] = 0x02; // endpoint 2

        let seq = self.tx_seq;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        self.host_pkt_buf[4..8].copy_from_slice(&seq.to_be_bytes());

        // Fill two sub-frames with control + TX data
        // Take buffer out to avoid borrow conflict
        let mut pkt = std::mem::take(&mut self.host_pkt_buf);
        self.fill_host_subframe(&mut pkt, 8);
        self.fill_host_subframe(&mut pkt, 520);
        self.host_pkt_buf = pkt;

        self.transport
            .send_to(&self.host_pkt_buf, self.device.addr)
            .await?;
        Ok(())
    }

    fn fill_host_subframe(&mut self, buf: &mut [u8], offset: usize) {
        buf[offset..offset + 3].copy_from_slice(&SYNC);

        // Rotate through control addresses
        let mox_bit = if self.ptt { 0x01 } else { 0x00 };
        let (c0, c1, c2, c3, c4) = self.next_control_command();
        buf[offset + 3] = c0 | mox_bit;
        buf[offset + 4] = c1;
        buf[offset + 5] = c2;
        buf[offset + 6] = c3;
        buf[offset + 7] = c4;

        // TX IQ data: 63 blocks of 8 bytes each [L(2B) R(2B) I(2B) Q(2B)]
        // Pop from lock-free ring buffer consumer when PTT is on
        if self.ptt {
            // Refill buffer from ring buffer if we need more samples
            if self.tx_iq_buffer.len() < 63 {
                if let Some(ref mut cons) = self.tx_iq_consumer {
                    let avail = cons.occupied_len();
                    if avail > 0 {
                        let old_len = self.tx_iq_buffer.len();
                        self.tx_iq_buffer
                            .resize(old_len + avail, Complex::new(0.0, 0.0));
                        let popped = cons.pop_slice(&mut self.tx_iq_buffer[old_len..]);
                        self.tx_iq_buffer.truncate(old_len + popped);
                    }
                }
            }

            let n = self.tx_iq_buffer.len().min(63);
            let mut peak = 0.0f64;
            if n > 0 {
                for i in 0..n {
                    let sample = self.tx_iq_buffer[i];
                    let base = offset + 8 + i * 8;
                    if base + 8 > buf.len() {
                        break;
                    }
                    // L/R audio (mic): zero in CW mode (tone lives in IQ only),
                    // otherwise send real part as 16-bit mono on both channels.
                    if !self.tx_cw_mode {
                        let mic_val = (sample.re.clamp(-1.0, 1.0) * 32767.0) as i16;
                        buf[base..base + 2].copy_from_slice(&mic_val.to_be_bytes());
                        buf[base + 2..base + 4].copy_from_slice(&mic_val.to_be_bytes());
                    }
                    // I/Q: 16-bit complex — Q is negated to match HPSDR wire convention.
                    // The emulator uses non-negated Q internally but this has been
                    // validated to work correctly with real HPSDR hardware.
                    let i_val = (sample.re.clamp(-1.0, 1.0) * 32767.0) as i16;
                    let q_val = ((-sample.im).clamp(-1.0, 1.0) * 32767.0) as i16;
                    buf[base + 4..base + 6].copy_from_slice(&i_val.to_be_bytes());
                    buf[base + 6..base + 8].copy_from_slice(&q_val.to_be_bytes());
                    let re_abs = sample.re.abs();
                    let im_abs = sample.im.abs();
                    peak = peak.max(re_abs.max(im_abs));
                    self.tx_samples_total += 1;
                    if re_abs > 1.0 {
                        self.tx_samples_clipped_re += 1;
                    }
                    if im_abs > 1.0 {
                        self.tx_samples_clipped_im += 1;
                    }
                    if re_abs > self.tx_max_excess {
                        self.tx_max_excess = re_abs;
                    }
                    if im_abs > self.tx_max_excess {
                        self.tx_max_excess = im_abs;
                    }
                }
                // Drain consumed samples
                self.tx_iq_buffer.drain(..n);

                if self.tx_subframes_with_data < 3 {
                    log::debug!(
                        "[P1 TX] Subframe packed: {} IQ samples, peak={:.4}, buffer_remaining={}",
                        n,
                        peak,
                        self.tx_iq_buffer.len()
                    );
                }
                self.tx_subframes_with_data += 1;
            } else {
                peak = 0.0;
                self.tx_subframes_empty += 1;
            }

            // Periodic debug log — every ~50 subframes (~131ms) to track
            // whether IQ gaps (peak=0) actually reach the wire during CW.
            self.tx_debug_log_counter += 1;
            if self.tx_debug_log_counter.is_multiple_of(50) {
                let total = self.tx_samples_total.max(1);
                let pct_clipped = (self.tx_samples_clipped_re + self.tx_samples_clipped_im) as f64
                    * 100.0
                    / (2 * total) as f64;
                log::debug!(
                    "[P1 TX WIRE] peak={:.6} n={} buf={} cw={} drive={} sf_data={} sf_empty={} skips={} clipped={:.3}% max_excess={:.4}",
                    peak,
                    n,
                    self.tx_iq_buffer.len(),
                    self.tx_cw_mode,
                    self.tx_drive,
                    self.tx_subframes_with_data,
                    self.tx_subframes_empty,
                    self.tx_underrun_skips,
                    pct_clipped,
                    self.tx_max_excess,
                );
            }
        }
    }

    /// Get the next control command in the rotation.
    fn next_control_command(&mut self) -> (u8, u8, u8, u8, u8) {
        let idx = self.control_idx;
        self.control_idx = (self.control_idx + 1) % 13;

        match idx {
            0 => {
                // Address 0x00: sample rate + nddc + duplex + OC outputs
                let rate_code = sample_rate_to_p1_code(self.sample_rate);
                let nddc_bits = ((self.nddc.max(1) - 1) & 0x07) << 3;
                let duplex = 0x04; // always duplex (required by HL2, harmless for others)
                                   // OC outputs for HL2 N2ADR filter board (C2[7:1])
                let c2 = match self.device.hw_type {
                    HpsdrHw::HermesLite => {
                        let freq = if self.ptt {
                            self.tx_frequency
                        } else {
                            self.rx_frequencies[0]
                        };
                        n2adr_oc_for_freq(freq) << 1
                    }
                    _ => 0,
                };
                (0x00, rate_code, c2, 0, nddc_bits | duplex)
            }
            1 => {
                // Address 0x02: TX frequency
                let b = self.tx_frequency.to_be_bytes();
                (0x02, b[0], b[1], b[2], b[3])
            }
            2..=8 => {
                // Addresses 0x04-0x10: RX frequencies
                let ddc = (idx - 2) as usize;
                let addr = 0x04 + (ddc as u8 * 2);
                let freq = if ddc < self.nddc as usize {
                    self.rx_frequencies[ddc]
                } else {
                    0
                };
                let b = freq.to_be_bytes();
                (addr, b[0], b[1], b[2], b[3])
            }
            9 => {
                // Address 0x12: TX drive + PA enable + Alex filters
                match self.device.hw_type {
                    HpsdrHw::HermesLite => {
                        // HL2: PA enable in C2, no Alex filters
                        (0x12, self.tx_drive, 0x08, 0, 0)
                    }
                    _ => {
                        // Standard HPSDR: Alex RX HPF in C3, Alex TX LPF in C4
                        let c3 = alex_rx_hpf_for_freq(self.rx_frequencies[0]);
                        let c4 = alex_tx_lpf_for_freq(self.tx_frequency);
                        (0x12, self.tx_drive, 0, c3, c4)
                    }
                }
            }
            10 => {
                // Address 0x14: RX attenuator
                match self.device.hw_type {
                    HpsdrHw::HermesLite => {
                        // HL2: step attenuator in C4 (direct value, 0-31)
                        (0x14, 0, 0, 0, self.rx_attenuation & 0x1F)
                    }
                    _ => {
                        // Standard: C4 bit 5 = 20dB attenuator
                        let c4 = if self.rx_attenuation >= 20 {
                            0x20
                        } else {
                            0x00
                        };
                        (0x14, 0, 0, 0, c4)
                    }
                }
            }
            11 => {
                // Address 0x16: step attenuator values and CW keyer settings
                (0x16, 0, 0, 0, 0)
            }
            12 => {
                // Address 0x1C: ADC assignments + TX attenuation
                match self.device.hw_type {
                    HpsdrHw::HermesLite => {
                        // HL2: C3 = 0xC0 | rx_gain (bit 7=enable, bit 6=full-range, bits 5:0=value)
                        (0x1C, 0, 0, 0xC0 | (self.rx_attenuation & 0x3F), 0)
                    }
                    _ => (0x1C, 0, 0, 0, 0),
                }
            }
            _ => (0x00, 0, 0, 0, 0),
        }
    }

    /// Handle an incoming packet from the radio.
    fn handle_radio_packet(&mut self, data: &[u8]) {
        if data.len() < 8 {
            return;
        }

        // Check magic bytes
        if data[0] != 0xEF || data[1] != 0xFE {
            return;
        }

        match data[2] {
            0x02 | 0x03 => {
                // Discovery response (ignore during streaming)
                log::debug!("P1 Discovery response received while streaming");
            }
            0x01 => {
                // Data packet from radio (endpoint 6)
                if data.len() >= PACKET_SIZE {
                    let seq = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                    if seq != self.rx_seq && self.rx_seq > 0 {
                        let lost = seq.wrapping_sub(self.rx_seq);
                        if lost > 1 && lost < 100 {
                            log::warn!(
                                "P1 Packet loss: expected seq {}, got {} ({} lost)",
                                self.rx_seq,
                                seq,
                                lost - 1
                            );
                        }
                    }
                    self.rx_seq = seq.wrapping_add(1);

                    // Parse both sub-frames
                    self.parse_radio_subframe(&data[8..520]);
                    self.parse_radio_subframe(&data[520..1032]);
                }
            }
            other => {
                log::warn!("P1 Unknown packet type 0x{:02X} from radio", other);
            }
        }
    }

    /// Parse a single radio-to-host sub-frame.
    fn parse_radio_subframe(&mut self, sf: &[u8]) {
        if sf.len() < SUBFRAME_SIZE || sf[0..3] != SYNC {
            return;
        }

        let c0 = sf[3];
        let c1 = sf[4];
        let c2 = sf[5];
        let c3 = sf[6];
        let c4 = sf[7];

        // Parse control response — accumulate across response address rotation.
        // The HPSDR P1 status subframe carries data for ONE address slot at a
        // time, rotating 0x00 → 0x08 → 0x10 → 0x18 → 0x00 → … Each slot
        // updates DIFFERENT fields of `accumulated_status`:
        //
        //   0x00 → adc_overflow
        //   0x08 → exciter_power, forward_power
        //   0x10 → reverse_power, supply_voltage
        //   0x18 → pa_current
        //
        // PTT is in c0 of every subframe, so we always update it. Detect
        // cycle wrap (current addr ≤ previous) and emit the just-completed
        // cycle's status BEFORE this subframe overwrites its fields. That
        // way every emitted RadioStatus has fwd, rev, supply, etc. all
        // sampled from the same rotation — SWR computed from a coherent
        // pair, not the half-fresh / half-stale mix we used to ship.
        let _ptt = (c0 & 0x01) != 0;
        let addr = c0 & 0x7E; // mask out PTT bit and bit 7
        self.accumulated_status.ptt = _ptt;

        let wrapped = self
            .last_status_addr
            .map(|prev| addr <= prev)
            .unwrap_or(false);
        if wrapped {
            // Cycle complete — emit BEFORE we mutate the new cycle's fields.
            let _ = self.status_sender.try_send(self.accumulated_status.clone());
        }
        self.last_status_addr = Some(addr);

        match addr {
            0x00 => {
                self.accumulated_status.adc_overflow = c1;
            }
            0x08 => {
                self.accumulated_status.exciter_power = u16::from_be_bytes([c1, c2]);
                self.accumulated_status.forward_power = u16::from_be_bytes([c3, c4]);
            }
            0x10 => {
                self.accumulated_status.reverse_power = u16::from_be_bytes([c1, c2]);
                self.accumulated_status.supply_voltage = u16::from_be_bytes([c3, c4]);
            }
            0x18 => {
                self.accumulated_status.pa_current = u16::from_be_bytes([c1, c2]);
            }
            _ => {}
        }

        // Extract IQ samples
        let nddc = self.nddc.max(1) as usize;
        let spr = 504 / (6 * nddc + 2);
        let block_size = 6 * nddc + 2;

        // Resize pre-allocated DDC buffers if nddc changed
        if self.ddc_sample_bufs.len() != nddc {
            self.ddc_sample_bufs.clear();
            for _ in 0..nddc {
                self.ddc_sample_bufs.push(Vec::with_capacity(spr));
            }
        } else {
            for buf in &mut self.ddc_sample_bufs {
                buf.clear();
                if buf.capacity() == 0 {
                    buf.reserve(spr);
                }
            }
        }

        // Deinterleave IQ for each DDC (reusing pre-allocated buffers)
        for row in 0..spr {
            let row_offset = 8 + row * block_size;
            for (ddc, ddc_vec) in self.ddc_sample_bufs.iter_mut().enumerate().take(nddc) {
                let sample_offset = row_offset + ddc * 6;
                if sample_offset + 6 <= sf.len() {
                    let sample = unpack_iq_24bit(sf, sample_offset);
                    ddc_vec.push(sample);
                }
            }
        }

        // Send to channel receivers — use mem::take to avoid cloning
        for (ddc, samples) in self.ddc_sample_bufs.iter_mut().enumerate() {
            if let Some(sender) = self.iq_senders.get(&ddc) {
                if !samples.is_empty() {
                    let _ = sender.try_send(std::mem::take(samples));
                }
            }
        }
    }
}

/// Build a Protocol 1 discovery request packet (63 bytes).
fn build_p1_discovery_request() -> Vec<u8> {
    let mut req = vec![0u8; 63];
    req[0] = 0xEF;
    req[1] = 0xFE;
    req[2] = 0x02;
    req
}

/// Parse a Protocol 1 discovery response.
fn parse_p1_discovery_response(data: &[u8], addr: SocketAddr) -> Option<DiscoveredDevice> {
    if data.len() < 20 {
        return None;
    }

    // Check magic bytes
    if data[0] != 0xEF || data[1] != 0xFE {
        return None;
    }

    // Status byte: 0x02 = normal, 0x03 = busy
    let status = data[2];
    if status != 0x02 && status != 0x03 {
        return None;
    }

    let mut mac = [0u8; 6];
    mac.copy_from_slice(&data[3..9]);

    let firmware_version = data[9];
    let device_code = data[10];
    let protocol_version = data[11];
    let num_rxs = if data.len() > 20 { data[20] } else { 1 };

    let protocol = if protocol_version == 0 {
        Protocol::Protocol1
    } else {
        Protocol::Protocol2
    };

    let hw_type = if protocol == Protocol::Protocol1 {
        HpsdrHw::from_p1_code(device_code)
    } else {
        HpsdrHw::from_p2_code(device_code)
    };

    // Use the radio's actual address (port 1024)
    let radio_addr = SocketAddr::new(addr.ip(), PORT);

    Some(DiscoveredDevice {
        addr: radio_addr,
        mac,
        hw_type,
        firmware_version,
        protocol,
        num_rxs,
        status,
    })
}
