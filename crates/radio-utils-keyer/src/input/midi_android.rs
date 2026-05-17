//! Android MIDI paddle input via AMidi (native NDK polling) + JNI for device discovery.
//!
//! Device enumeration and opening still go through Java's `MidiManager` API
//! (no alternative exists). Once the `MidiDevice` Java object is in hand, we
//! call `AMidiDevice_fromJava()` to obtain a native handle and then
//! `AMidiOutputPort_open()` to get an `AMidiOutputPort*`. A dedicated
//! native polling thread calls `AMidiOutputPort_receive()` in a tight loop
//! (~1 ms idle sleep, limited by Android scheduler granularity). No JVM
//! involvement on the hot receive path — no GC pauses, no thread-scheduler
//! interference from the JVM.
//!
//! Requires Android API 29 (AMidi).
#![cfg(target_os = "android")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{unbounded, Receiver, Sender};
use jni::objects::{GlobalRef, JClass, JValue};
use jni::{JNIEnv, JavaVM};
use std::sync::OnceLock;

use crate::config::MidiBinding;
use crate::input::midi::RawMidiEvent;
use crate::input::{PaddleInput, PaddleState};

// ---------------------------------------------------------------------------
// Global JVM state (set once from the Android entry point)
// ---------------------------------------------------------------------------

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();
static MIDI_HELPER_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Initialise the global JVM + Android Context references.
///
/// Must be called once from the Android entry-point before any MIDI API is used.
pub fn set_jvm(vm: JavaVM, context: GlobalRef) {
    JAVA_VM.set(vm).ok();
    CONTEXT.set(context).ok();
}

// ---------------------------------------------------------------------------
// Embedded DEX (built by build.rs; path injected via env var)
// ---------------------------------------------------------------------------

const RADIO_UTILS_KEYER_DEX: &[u8] = include_bytes!(env!("RADIO_UTILS_KEYER_DEX"));

use super::MidiSharedState as SharedState;

// ---------------------------------------------------------------------------
// AMidi FFI — API 29+
//
// Android naming is from the MIDI device's perspective:
//   OutputPort = data flowing OUT of the device → what the host reads
//   InputPort  = data flowing INTO the device → what the host writes
//
// We want to read paddle events sent by the MIDI device, so we open an
// OutputPort and call AMidiOutputPort_receive().
// ---------------------------------------------------------------------------

#[repr(C)]
struct AMidiDevice {
    _private: [u8; 0],
}

#[repr(C)]
struct AMidiOutputPort {
    _private: [u8; 0],
}

// Matches AMIDI_OPCODE_DATA from <amidi/AMidi.h> (NDK API 29).
const AMIDI_OPCODE_DATA: i32 = 1;

extern "C" {
    /// Obtain a native `AMidiDevice` from a Java `MidiDevice` object.
    /// Must be called on a thread with a valid JNI env.
    fn AMidiDevice_fromJava(
        env: *mut jni::sys::JNIEnv,
        midi_device_obj: jni::sys::jobject,
        out_device: *mut *mut AMidiDevice,
    ) -> i32; // media_status_t; 0 = AMEDIA_OK

    fn AMidiDevice_release(device: *mut AMidiDevice) -> i32;

    /// Open the device's output port (data flows from device to host).
    fn AMidiOutputPort_open(
        device: *mut AMidiDevice,
        port_number: i32,
        out_port: *mut *mut AMidiOutputPort,
    ) -> i32;

    fn AMidiOutputPort_close(port: *mut AMidiOutputPort) -> i32;

    /// Non-blocking receive. Returns number of messages received (0 or 1),
    /// or a negative error code. Actual byte count is in `num_bytes_received`.
    fn AMidiOutputPort_receive(
        port: *mut AMidiOutputPort,
        opcode: *mut i32,
        buffer: *mut u8,
        max_bytes: usize,
        num_bytes_received: *mut usize,
        timestamp: *mut i64,
    ) -> isize;
}

// ---------------------------------------------------------------------------
// Send-safe raw-pointer wrappers (owned exclusively by the poll thread)
// ---------------------------------------------------------------------------

struct SendPtr<T>(*mut T);
unsafe impl<T> Send for SendPtr<T> {}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

/// Paddle input backed by an Android MIDI device (AMidi native polling backend).
pub struct MidiAndroidInput {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    /// Java `MidiHelper` object — keeps the underlying `MidiDevice` open.
    helper_ref: GlobalRef,
    stop: Arc<AtomicBool>,
    poll_thread: Option<std::thread::JoinHandle<()>>,
}

impl MidiAndroidInput {
    /// Open the named Android MIDI device and start the native AMidi polling thread.
    ///
    /// Returns `Ok((input, monitor_rx))` on success. `monitor_rx` receives every
    /// raw MIDI event for the settings-UI learn mode.
    pub fn new(
        device_name: &str,
        dit_binding: Option<MidiBinding>,
        dah_binding: Option<MidiBinding>,
    ) -> Result<(Self, Receiver<RawMidiEvent>), String> {
        // --- acquire global JVM / Context -----------------------------------
        let vm = JAVA_VM
            .get()
            .ok_or("JAVA_VM not initialised — call set_jvm() first")?;
        let ctx = CONTEXT
            .get()
            .ok_or("CONTEXT not initialised — call set_jvm() first")?;

        let mut env = vm
            .attach_current_thread()
            .map_err(|e| format!("attach_current_thread: {e}"))?;

        // --- load MidiHelper class from embedded DEX -------------------------
        let helper_class = load_midi_helper_class(&mut env)?;

        // --- call MidiHelper.openDevice(context, name) ----------------------
        let j_name = env
            .new_string(device_name)
            .map_err(|e| format!("new_string device_name: {e}"))?;

        let helper_obj = env
            .call_static_method(
                &helper_class,
                "openDevice",
                "(Landroid/content/Context;Ljava/lang/String;)Lcom/radioutils/midi/MidiHelper;",
                &[JValue::Object(ctx.as_obj()), JValue::Object(&j_name)],
            )
            .map_err(|e| {
                let _ = env.exception_clear();
                format!("MidiHelper.openDevice: {e}")
            })?
            .l()
            .map_err(|e| format!("openDevice not object: {e}"))?;

        if helper_obj.is_null() {
            return Err(format!(
                "MidiHelper.openDevice returned null for '{device_name}'"
            ));
        }

        // --- get native AMidiDevice* via AMidiDevice_fromJava ---------------
        let midi_device_obj = env
            .call_method(
                &helper_obj,
                "getMidiDevice",
                "()Landroid/media/midi/MidiDevice;",
                &[],
            )
            .map_err(|e| {
                let _ = env.exception_clear();
                format!("getMidiDevice: {e}")
            })?
            .l()
            .map_err(|e| format!("getMidiDevice not object: {e}"))?;

        if midi_device_obj.is_null() {
            return Err("getMidiDevice returned null".to_string());
        }

        let amidi_device: *mut AMidiDevice = {
            let mut ptr: *mut AMidiDevice = std::ptr::null_mut();
            let status =
                unsafe { AMidiDevice_fromJava(env.get_raw(), midi_device_obj.as_raw(), &mut ptr) };
            if status != 0 || ptr.is_null() {
                return Err(format!("AMidiDevice_fromJava failed: status={status}"));
            }
            log::debug!("[MIDI] AMidiDevice_fromJava ok, device={ptr:p}");
            ptr
        };

        // --- open the first available output port (data flows from device to host) --
        // Try port indices 0..=7 and take the first that opens successfully.
        // Most CW interfaces expose a single port, but multi-port devices exist.
        let amidi_port: *mut AMidiOutputPort = {
            let mut found: Option<*mut AMidiOutputPort> = None;
            for port_num in 0..=7i32 {
                let mut ptr: *mut AMidiOutputPort = std::ptr::null_mut();
                let status = unsafe { AMidiOutputPort_open(amidi_device, port_num, &mut ptr) };
                if status == 0 && !ptr.is_null() {
                    log::debug!(
                        "[MIDI] AMidiOutputPort_open ok, port_number={port_num} ptr={ptr:p}"
                    );
                    found = Some(ptr);
                    break;
                }
            }
            match found {
                Some(ptr) => ptr,
                None => {
                    unsafe { AMidiDevice_release(amidi_device) };
                    return Err("AMidiOutputPort_open failed on all port indices 0..=7".to_string());
                }
            }
        };

        // --- set up shared state and channels --------------------------------
        let shared = Arc::new((
            Mutex::new(SharedState {
                dit: false,
                dash: false,
                timestamp: Instant::now(),
                generation: 0,
            }),
            Condvar::new(),
        ));

        let (monitor_tx, monitor_rx): (Sender<RawMidiEvent>, Receiver<RawMidiEvent>) = unbounded();

        // --- keep Java MidiHelper alive (holds MidiDevice open) -------------
        // SAFETY: on error, close native handles before propagating.
        let helper_ref = env.new_global_ref(helper_obj).map_err(|e| {
            unsafe {
                AMidiOutputPort_close(amidi_port);
                AMidiDevice_release(amidi_device);
            }
            format!("new_global_ref helper: {e}")
        })?;

        // --- spawn native polling thread -------------------------------------
        let stop = Arc::new(AtomicBool::new(false));
        let poll_thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let port_ptr = SendPtr(amidi_port);
            let device_ptr = SendPtr(amidi_device);

            std::thread::Builder::new()
                .name("midi-amidi-poll".into())
                .spawn(move || {
                    amidi_poll_thread(
                        port_ptr,
                        device_ptr,
                        stop,
                        shared,
                        dit_binding,
                        dah_binding,
                        monitor_tx,
                    );
                })
                .map_err(|e| format!("spawn midi poll thread: {e}"))?
        };

        Ok((
            Self {
                shared,
                helper_ref,
                stop,
                poll_thread: Some(poll_thread),
            },
            monitor_rx,
        ))
    }
}

// ---------------------------------------------------------------------------
// Native AMidi polling thread
// ---------------------------------------------------------------------------

fn amidi_poll_thread(
    port_ptr: SendPtr<AMidiOutputPort>,
    device_ptr: SendPtr<AMidiDevice>,
    stop: Arc<AtomicBool>,
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    dit_binding: Option<MidiBinding>,
    dah_binding: Option<MidiBinding>,
    monitor_tx: Sender<RawMidiEvent>,
) {
    // Elevate thread priority so the poll loop isn't preempted by the scheduler.
    set_thread_priority_audio();

    let port = port_ptr.0;
    log::debug!("[MIDI poll] thread started, port={port:p}");

    let mut buf = [0u8; 64];
    let mut opcode: i32 = 0;
    let mut num_bytes: usize = 0;
    let mut timestamp: i64 = 0;

    while !stop.load(Ordering::Relaxed) {
        // Returns number of messages received (0 or 1), not byte count.
        // Actual bytes written to buf are in num_bytes.
        let msgs = unsafe {
            AMidiOutputPort_receive(
                port,
                &mut opcode,
                buf.as_mut_ptr(),
                buf.len(),
                &mut num_bytes,
                &mut timestamp,
            )
        };

        if msgs > 0 {
            log::debug!(
                "[MIDI poll] received {} bytes opcode={opcode} raw={:?}",
                num_bytes,
                &buf[..num_bytes]
            );
        }

        if msgs > 0 && opcode == AMIDI_OPCODE_DATA {
            let bytes = &buf[..num_bytes];
            if let Some(ev) = RawMidiEvent::from_bytes(bytes) {
                log::debug!("[MIDI poll] parsed event: {:?}", ev);
                let _ = monitor_tx.try_send(ev);

                let (lock, cvar) = &*shared;
                if let Ok(mut s) = lock.lock() {
                    let mut dit = s.dit;
                    let mut dash = s.dash;
                    if let Some(ref b) = dit_binding {
                        if ev.matches(b) {
                            dit = ev.is_down;
                        }
                    }
                    if let Some(ref b) = dah_binding {
                        if ev.matches(b) {
                            dash = ev.is_down;
                        }
                    }
                    if s.dit != dit || s.dash != dash {
                        s.dit = dit;
                        s.dash = dash;
                        s.timestamp = Instant::now();
                        s.generation = s.generation.wrapping_add(1);
                        cvar.notify_one();
                    }
                }
            } else {
                log::debug!(
                    "[MIDI poll] unrecognised bytes (not NoteOn/Off/CC): {:?}",
                    bytes
                );
            }
            // No sleep — immediately poll again in case more messages are queued.
        } else if msgs == 0 {
            // No data available; sleep to avoid burning CPU.
            // Android scheduler granularity is ~1ms, so this sleeps ~1ms regardless
            // of the requested duration.
            std::thread::sleep(Duration::from_millis(1));
        } else {
            // Negative return: port error (device disconnected, closed, etc.).
            // Stop polling — the keyer will time out on wait_for_change.
            log::warn!("[MIDI poll] AMidiOutputPort_receive returned {msgs}, stopping poll thread");
            break;
        }
    }

    log::debug!("[MIDI poll] thread stopping, closing port={port:p}");

    // AMidiOutputPort_close and AMidiDevice_release both access the JVM internally.
    // This thread may be detached; attach temporarily for the cleanup calls.
    let _jni_guard = JAVA_VM.get().and_then(|vm| vm.attach_current_thread().ok());
    unsafe {
        AMidiOutputPort_close(port);
        AMidiDevice_release(device_ptr.0);
    }
    log::debug!("[MIDI poll] thread exited cleanly");
}

// ---------------------------------------------------------------------------
// PaddleInput impl
// ---------------------------------------------------------------------------

impl PaddleInput for MidiAndroidInput {
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap();
        let seen_gen = state.generation;
        let t = timeout.unwrap_or(Duration::from_secs(1));
        let deadline = Instant::now() + t;
        while state.generation == seen_gen {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let (g, timed_out) = cvar.wait_timeout(state, remaining).unwrap();
            state = g;
            if timed_out.timed_out() {
                break;
            }
        }
        PaddleState {
            dit: state.dit,
            dash: state.dash,
            timestamp: state.timestamp,
        }
    }

    fn read(&self) -> PaddleState {
        let (lock, _) = &*self.shared;
        let state = lock.lock().unwrap();
        PaddleState {
            dit: state.dit,
            dash: state.dash,
            timestamp: state.timestamp,
        }
    }

    fn describe(&self) -> String {
        "Android AMidi paddle input".to_string()
    }
}

// ---------------------------------------------------------------------------
// Thread priority helper
// ---------------------------------------------------------------------------

/// Set the calling thread's priority to `THREAD_PRIORITY_AUDIO` (-16).
///
/// No-op if the JVM has not been initialised.
/// Must be called from the thread whose priority should be elevated.
pub fn set_thread_priority_audio() {
    let Some(vm) = JAVA_VM.get() else { return };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let _ = env.call_static_method(
        "android/os/Process",
        "setThreadPriority",
        "(I)V",
        &[jni::objects::JValue::Int(-16)], // THREAD_PRIORITY_AUDIO
    );
    let _ = env.exception_clear();
}

// ---------------------------------------------------------------------------
// Drop: signal poll thread and close Java resources
// ---------------------------------------------------------------------------

impl Drop for MidiAndroidInput {
    fn drop(&mut self) {
        // Signal polling thread to stop and wait for it to close native handles.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.poll_thread.take() {
            let _ = handle.join();
        }
        // Release Java MidiDevice.
        if let Some(vm) = JAVA_VM.get() {
            if let Ok(mut env) = vm.attach_current_thread() {
                let _ = env.call_method(self.helper_ref.as_obj(), "close", "()V", &[]);
                let _ = env.exception_clear();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public: list available MIDI input device names
// ---------------------------------------------------------------------------

/// Return the names of all available Android MIDI input devices.
///
/// Returns an empty `Vec` if the JVM/Context have not been set or any JNI
/// call fails.
pub fn list_midi_input_devices() -> Vec<String> {
    let vm = match JAVA_VM.get() {
        Some(v) => v,
        None => return Vec::new(),
    };
    let ctx = match CONTEXT.get() {
        Some(c) => c,
        None => return Vec::new(),
    };

    let mut env = match vm.attach_current_thread() {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };

    let helper_class = match load_midi_helper_class(&mut env) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[MIDI] Failed to load MidiHelper class for listing: {e}");
            return Vec::new();
        }
    };

    let names_result = match env.call_static_method(
        &helper_class,
        "listDevices",
        "(Landroid/content/Context;)[Ljava/lang/String;",
        &[JValue::Object(ctx.as_obj())],
    ) {
        Ok(v) => v,
        Err(_) => {
            let _ = env.exception_clear();
            return Vec::new();
        }
    };

    let arr_obj = match names_result.l() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    if arr_obj.is_null() {
        return Vec::new();
    }

    let arr = unsafe { jni::objects::JObjectArray::from_raw(arr_obj.into_raw()) };

    let len = match env.get_array_length(&arr) {
        Ok(n) => n,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::with_capacity(len as usize);
    for i in 0..len {
        let elem = match env.get_object_array_element(&arr, i) {
            Ok(o) => o,
            Err(_) => continue,
        };
        if elem.is_null() {
            continue;
        }
        let jstr = unsafe { jni::objects::JString::from_raw(elem.into_raw()) };
        if let Ok(s) = env.get_string(&jstr) {
            result.push(s.into());
        };
    }

    result
}

// ---------------------------------------------------------------------------
// Private: load MidiHelper class from embedded DEX via InMemoryDexClassLoader
// ---------------------------------------------------------------------------

fn load_midi_helper_class<'a>(env: &mut JNIEnv<'a>) -> Result<JClass<'a>, String> {
    if let Some(cached) = MIDI_HELPER_CLASS.get() {
        let local = env
            .new_local_ref(cached.as_obj())
            .map_err(|e| format!("new_local_ref for cached class: {e}"))?;
        return Ok(unsafe { JClass::from_raw(local.into_raw()) });
    }

    // 1. Create jbyteArray from RADIO_UTILS_KEYER_DEX
    let len = RADIO_UTILS_KEYER_DEX.len() as i32;
    let arr = env
        .new_byte_array(len)
        .map_err(|e| format!("new_byte_array: {e}"))?;
    let bytes_i8: &[i8] = unsafe {
        std::slice::from_raw_parts(
            RADIO_UTILS_KEYER_DEX.as_ptr() as *const i8,
            RADIO_UTILS_KEYER_DEX.len(),
        )
    };
    env.set_byte_array_region(&arr, 0, bytes_i8)
        .map_err(|e| format!("set_byte_array_region: {e}"))?;

    // 2. ByteBuffer.wrap(byte[])
    let byte_buffer = env
        .call_static_method(
            "java/nio/ByteBuffer",
            "wrap",
            "([B)Ljava/nio/ByteBuffer;",
            &[JValue::Object(&arr.into())],
        )
        .map_err(|e| {
            let _ = env.exception_clear();
            format!("ByteBuffer.wrap: {e}")
        })?
        .l()
        .map_err(|e| format!("ByteBuffer not object: {e}"))?;

    // 3. Get parent class loader from the current thread
    let thread = env
        .call_static_method(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .map_err(|e| {
            let _ = env.exception_clear();
            format!("currentThread: {e}")
        })?
        .l()
        .map_err(|e| format!("thread not object: {e}"))?;

    let parent_cl = env
        .call_method(
            &thread,
            "getContextClassLoader",
            "()Ljava/lang/ClassLoader;",
            &[],
        )
        .map_err(|e| {
            let _ = env.exception_clear();
            format!("getContextClassLoader: {e}")
        })?
        .l()
        .map_err(|e| format!("classloader not object: {e}"))?;

    // 4. new InMemoryDexClassLoader(ByteBuffer, ClassLoader)
    let loader = env
        .new_object(
            "dalvik/system/InMemoryDexClassLoader",
            "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
            &[JValue::Object(&byte_buffer), JValue::Object(&parent_cl)],
        )
        .map_err(|e| {
            let _ = env.exception_clear();
            format!("InMemoryDexClassLoader: {e}")
        })?;

    // 5. loader.loadClass("com.radioutils.midi.MidiHelper")
    let class_name = env
        .new_string("com.radioutils.midi.MidiHelper")
        .map_err(|e| format!("new_string class name: {e}"))?;

    let class_obj = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&class_name)],
        )
        .map_err(|e| {
            let _ = env.exception_clear();
            format!("loadClass: {e}")
        })?
        .l()
        .map_err(|e| format!("loadClass not object: {e}"))?;

    // Cache as global ref.
    let global = env
        .new_global_ref(&class_obj)
        .map_err(|e| format!("global ref for class: {e}"))?;
    MIDI_HELPER_CLASS.set(global).ok();

    let cached = MIDI_HELPER_CLASS
        .get()
        .expect("MIDI_HELPER_CLASS was just initialised");
    let local = env
        .new_local_ref(cached.as_obj())
        .map_err(|e| format!("new_local_ref for cached class after set: {e}"))?;
    Ok(unsafe { JClass::from_raw(local.into_raw()) })
}
