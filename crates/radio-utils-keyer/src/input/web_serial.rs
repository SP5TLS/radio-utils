//! WASM WebSerial paddle input.
//!
//! Port selection: user clicks "Select Port…" → triggers `requestPort()` (browser dialog).
//! Signal polling: calls `port.getSignals()` at ~16ms cadence via gloo_timers Interval.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::js_sys;
use web_sys::SerialPort;

use crate::config::SerialPin;

/// Baud rate used when opening the WebSerial port.
/// The actual baud rate does not affect modem-control pin polling (CTS/DSR/DCD/RI),
/// but most USB serial adapters require a valid value to open successfully.
const SERIAL_BAUD_RATE: u32 = 9600;

/// Snapshot of the two paddle signals from the serial port.
#[derive(Default, Clone, Copy)]
pub struct SerialPaddleState {
    pub dit: bool,
    pub dash: bool,
}

/// WASM WebSerial paddle input.
///
/// On the web there is no device dropdown — the browser shows its own port picker dialog
/// when `request_port()` is called. Call `current_state()` each frame for the latest state.
///
/// The polling interval is owned by this struct; dropping it (or calling `request_port()`
/// again) stops the previous polling loop automatically.
pub struct WebSerialPaddleInput {
    state: Rc<RefCell<SerialPaddleState>>,
    dit_pin: SerialPin,
    dash_pin: SerialPin,
    /// Holds the active polling interval. Dropping it cancels the timer.
    _interval: Rc<RefCell<Option<gloo_timers::callback::Interval>>>,
}

impl WebSerialPaddleInput {
    pub fn new(dit_pin: SerialPin, dash_pin: SerialPin) -> Self {
        Self {
            state: Rc::new(RefCell::new(SerialPaddleState::default())),
            dit_pin,
            dash_pin,
            _interval: Rc::new(RefCell::new(None)),
        }
    }

    /// Trigger the browser port picker dialog. After the user selects a port, opens it and
    /// starts polling `getSignals()` at ~16ms.
    ///
    /// If a port was previously selected, its polling interval is stopped before the new
    /// picker is shown.
    pub fn request_port(&mut self) {
        // Drop the existing interval to stop any previous polling loop.
        *self._interval.borrow_mut() = None;

        let window = match web_sys::window() {
            Some(w) => w,
            None => return, // not running in a browser context
        };
        let serial = window.navigator().serial();
        // Use the no-arg overload (no filter options) so only `Serial` feature is required.
        let promise = serial.request_port();

        let state_clone = Rc::clone(&self.state);
        let dit_pin = self.dit_pin;
        let dash_pin = self.dash_pin;
        let interval_holder = Rc::clone(&self._interval);

        let success = Closure::once(move |port: SerialPort| {
            start_polling(port, state_clone, dit_pin, dash_pin, interval_holder);
        });
        let _ = promise.then(&success);
        success.forget();
    }

    /// Returns the last-polled paddle state.
    pub fn current_state(&self) -> SerialPaddleState {
        *self.state.borrow()
    }
}

fn start_polling(
    port: SerialPort,
    state: Rc<RefCell<SerialPaddleState>>,
    dit_pin: SerialPin,
    dash_pin: SerialPin,
    interval_holder: Rc<RefCell<Option<gloo_timers::callback::Interval>>>,
) {
    let open_opts = web_sys::SerialOptions::new(SERIAL_BAUD_RATE);
    let open_promise = port.open(&open_opts);
    let port_clone = port.clone();
    let state_clone = Rc::clone(&state);

    let opened = Closure::once(move |_: js_sys::Undefined| {
        let interval = gloo_timers::callback::Interval::new(16, move || {
            let signals_promise = port_clone.get_signals();
            let state_for_cb = Rc::clone(&state_clone);
            wasm_bindgen_futures::spawn_local(async move {
                if let Ok(signals) = wasm_bindgen_futures::JsFuture::from(signals_promise).await {
                    let cts = signals.get_clear_to_send();
                    let dsr = signals.get_data_set_ready();
                    let dcd = signals.get_data_carrier_detect();
                    let ri = signals.get_ring_indicator();
                    let get = |pin: SerialPin| match pin {
                        SerialPin::CTS => cts,
                        SerialPin::DSR => dsr,
                        SerialPin::DCD => dcd,
                        SerialPin::RI => ri,
                    };
                    let mut s = state_for_cb.borrow_mut();
                    s.dit = get(dit_pin);
                    s.dash = get(dash_pin);
                }
            });
        });
        // Store the interval in the holder instead of leaking it, so it can be cancelled
        // by dropping the WebSerialPaddleInput or calling request_port() again.
        *interval_holder.borrow_mut() = Some(interval);
    });
    let open_err = Closure::once(|err: wasm_bindgen::JsValue| {
        web_sys::console::warn_1(&format!("WebSerial open failed: {:?}", err).into());
    });
    // Chain: opened fires on success, open_err fires if port.open() rejects.
    let _ = open_promise.then(&opened).catch(&open_err);
    opened.forget();
    open_err.forget();
}
