//! WASM MIDI paddle input via WebMIDI API.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use web_sys::{MidiAccess, MidiInput, MidiMessageEvent};

use crate::config::MidiBinding;
use crate::input::midi::RawMidiEvent;

/// Shared event queue. Clone the `Rc` to share with the settings panel learn mode.
pub type MidiEventQueue = Rc<RefCell<VecDeque<RawMidiEvent>>>;

/// WASM MIDI paddle input. Call `init()` after construction (async JS Promise).
///
/// This is NOT a `PaddleInput` implementor — on WASM the update loop manually OR-merges
/// sources. The update loop calls `drain_events()` each frame.
pub struct WebMidiPaddleInput {
    /// Shared with the settings panel for learn mode monitoring.
    pub event_queue: MidiEventQueue,
    /// Device names, populated after `init()` resolves.
    pub device_names: Rc<RefCell<Vec<String>>>,
    dit_binding: Option<MidiBinding>,
    dah_binding: Option<MidiBinding>,
    /// Name of the configured device; only this device gets a listener.
    device_name: Option<String>,
    _callbacks: Vec<Closure<dyn FnMut(MidiMessageEvent)>>,
    /// Persistent paddle state — updated by events, held between frames.
    dit_state: bool,
    dash_state: bool,
    /// Stored MidiAccess so we can re-enumerate devices and re-attach listeners.
    access: Rc<RefCell<Option<MidiAccess>>>,
}

impl WebMidiPaddleInput {
    pub fn new(
        dit_binding: Option<MidiBinding>,
        dah_binding: Option<MidiBinding>,
        device_name: Option<String>,
    ) -> Self {
        Self {
            event_queue: Rc::new(RefCell::new(VecDeque::new())),
            device_names: Rc::new(RefCell::new(Vec::new())),
            dit_binding,
            dah_binding,
            device_name,
            _callbacks: Vec::new(),
            dit_state: false,
            dash_state: false,
            access: Rc::new(RefCell::new(None)),
        }
    }

    /// Update dit/dah bindings and device name after MIDI learn completes.
    ///
    /// Binding changes (without a device change) take effect immediately on the
    /// next [`drain_events`](Self::drain_events) call because the event callback
    /// pushes all raw events to the queue and the filter is applied in `drain_events`.
    ///
    /// If the device name also changed and `init()` has already resolved, the old
    /// `onmidimessage` listener is removed and a new one is attached to the
    /// new device — no page reload required.
    pub fn update_bindings(
        &mut self,
        dit_binding: Option<MidiBinding>,
        dah_binding: Option<MidiBinding>,
        device_name: Option<String>,
    ) {
        let device_changed = device_name != self.device_name;
        self.dit_binding = dit_binding;
        self.dah_binding = dah_binding;
        self.device_name = device_name;

        if device_changed {
            let access_clone = self.access.borrow().clone();
            if let Some(access) = access_clone {
                self.setup_inputs(&access);
            }
        }
    }

    /// Call once after construction. Resolves via JS microtask queue.
    /// Returns `false` if the browser does not support WebMIDI (e.g. Firefox).
    pub fn init(this: Rc<RefCell<Self>>) -> bool {
        let window = match web_sys::window() {
            Some(w) => w,
            None => return false,
        };
        let navigator = window.navigator();

        let promise = match navigator.request_midi_access() {
            Ok(p) => p,
            Err(e) => {
                web_sys::console::warn_1(&format!("WebMIDI not available: {:?}", e).into());
                return false;
            }
        };

        let this_clone = Rc::clone(&this);
        let success = Closure::once(move |access: JsValue| {
            let access: MidiAccess = match access.dyn_into() {
                Ok(a) => a,
                Err(_) => {
                    web_sys::console::warn_1(&"WebMIDI: unexpected access object type".into());
                    return;
                }
            };
            let mut inner = this_clone.borrow_mut();
            inner.setup_inputs(&access);
        });
        let _ = promise.then(&success);
        success.forget();
        true
    }

    fn setup_inputs(&mut self, access: &MidiAccess) {
        // First pass: unregister all existing onmidimessage handlers so the old
        // JS closures are not called after we drop them below.
        for entry in access.inputs().values() {
            let Ok(js_val) = entry else { continue };
            let Ok(input) = js_val.dyn_into::<MidiInput>() else {
                continue;
            };
            input.set_onmidimessage(None);
        }
        // Safe to drop old closures now that no port references them.
        self._callbacks.clear();

        // Second pass: enumerate names and attach listener to the target device.
        let mut names = Vec::new();
        for entry in access.inputs().values() {
            let Ok(js_val) = entry else { continue };
            let Ok(input) = js_val.dyn_into::<MidiInput>() else {
                continue;
            };
            let name = input
                .name()
                .unwrap_or_else(|| "Unknown MIDI Device".to_string());
            names.push(name.clone());

            let is_target = self
                .device_name
                .as_deref()
                .map(|d| d == name)
                .unwrap_or(false);

            if is_target {
                let queue = Rc::clone(&self.event_queue);
                let cb = Closure::<dyn FnMut(MidiMessageEvent)>::new(move |e: MidiMessageEvent| {
                    let arr = match e.data() {
                        Ok(a) => a,
                        Err(_) => return, // no payload (e.g. active sensing without data)
                    };
                    let bytes = arr.to_vec();
                    if let Some(ev) = RawMidiEvent::from_bytes(&bytes) {
                        queue.borrow_mut().push_back(ev);
                    }
                });
                input.set_onmidimessage(Some(cb.as_ref().unchecked_ref()));
                self._callbacks.push(cb);
            }
        }
        *self.device_names.borrow_mut() = names;
        *self.access.borrow_mut() = Some(access.clone());
    }

    /// Re-enumerate connected MIDI devices and update `device_names`.
    /// Call when the user clicks the "↺ Refresh" button.
    pub fn refresh_device_names(&self) {
        if let Some(ref access) = *self.access.borrow() {
            let mut names = Vec::new();
            for entry in access.inputs().values() {
                let Ok(js_val) = entry else { continue };
                let Ok(input) = js_val.dyn_into::<MidiInput>() else {
                    continue;
                };
                names.push(input.name().unwrap_or_default());
            }
            *self.device_names.borrow_mut() = names;
        }
    }

    /// Drain all pending events and return `(dit, dash, all_events)`.
    ///
    /// `dit`/`dash` reflect the **current held state** — they remain `true` across frames
    /// while the key is held and only clear when the corresponding Note Off / CC=0 arrives.
    pub fn drain_events(&mut self) -> (bool, bool, Vec<RawMidiEvent>) {
        let mut all_events = Vec::new();
        let mut q = self.event_queue.borrow_mut();
        while let Some(ev) = q.pop_front() {
            if let Some(ref b) = self.dit_binding {
                if ev.matches(b) {
                    self.dit_state = ev.is_down;
                }
            }
            if let Some(ref b) = self.dah_binding {
                if ev.matches(b) {
                    self.dash_state = ev.is_down;
                }
            }
            all_events.push(ev);
        }
        (self.dit_state, self.dash_state, all_events)
    }
}
