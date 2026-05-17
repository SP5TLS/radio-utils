//! Android USB-serial paddle input — JNI bridge to `com.radioutils.serial.UsbSerialHelper`.
//!
//! Latency design:
//! - One dedicated polling thread, attached to the JVM as a daemon so no
//!   per-iteration JNI attach/detach happens.
//! - `JMethodID` for `readModemStatus()` is resolved once at open time and
//!   reused via `call_method_unchecked` (skips per-call `GetMethodID` lookup).
//! - The thread runs at `THREAD_PRIORITY_AUDIO` (-16) — same elevation as
//!   the AMidi poll thread.
//! - On vendor chips (FTDI / CH340 / CP210x) each poll is a single USB
//!   control transfer that blocks for one USB SOF (~1 ms on full-speed
//!   USB-OTG). Once the modem state has been stable for >250 ms the loop
//!   adds a 5 ms backoff sleep so an idle paddle doesn't pin the USB bus
//!   at 1 kHz indefinitely. Active paddling resumes at the bus-natural
//!   ~1 ms cadence the moment any line flips. Keyer consumers are notified
//!   through a condvar.
//!
//! Java side: see `java/UsbSerialHelper.java`. Bundled
//! into the same DEX as `MidiHelper` by `build.rs`.
#![cfg(target_os = "android")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jni::objects::{GlobalRef, JClass, JMethodID, JValue};
use jni::signature::{Primitive, ReturnType};
use jni::sys::jmethodID;
use jni::{JNIEnv, JavaVM};

use crate::config::SerialPin;
use crate::input::{PaddleInput, PaddleState};

// ---------------------------------------------------------------------------
// Global JVM state (shared with midi_android via separate OnceLocks)
// ---------------------------------------------------------------------------

static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
static CONTEXT: OnceLock<GlobalRef> = OnceLock::new();
static HELPER_CLASS: OnceLock<GlobalRef> = OnceLock::new();

/// Same DEX as the MIDI helper — `build.rs` bundles every `.java` file in
/// `java/` into one `classes.dex`.
const RADIO_UTILS_KEYER_DEX: &[u8] = include_bytes!(env!("RADIO_UTILS_KEYER_DEX"));

/// Initialise the global JVM + Android Context references for the USB-serial
/// bridge. Called once from the Android entry point alongside the equivalent
/// calls for `service_bridge` and `midi_android`.
pub fn set_jvm(vm: JavaVM, ctx: GlobalRef) {
    JAVA_VM.set(vm).ok();
    CONTEXT.set(ctx).ok();
}

// ---------------------------------------------------------------------------
// Send-safe method ID wrapper
// ---------------------------------------------------------------------------

/// Send wrapper for `jmethodID` so the poll thread can carry it across the
/// thread boundary. `jmethodID` is a process-wide opaque pointer and is safe
/// to use from any thread that has the JVM attached.
struct SendMid(jmethodID);
unsafe impl Send for SendMid {}

// ---------------------------------------------------------------------------
// Shared paddle state
// ---------------------------------------------------------------------------

struct SharedState {
    dit: bool,
    dash: bool,
    timestamp: Instant,
    /// Bumped on every observed state change; lets `wait_for_change` ignore
    /// spurious condvar wakeups.
    generation: u64,
}

// ---------------------------------------------------------------------------
// Public struct
// ---------------------------------------------------------------------------

pub struct SerialPaddleInput {
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    /// Java `UsbSerialHelper` — kept alive for the lifetime of the input so
    /// the underlying `UsbDeviceConnection` stays open. Closed in `Drop`.
    helper_ref: GlobalRef,
    stop: Arc<AtomicBool>,
    poll_thread: Option<JoinHandle<()>>,
    port_name: String,
    dit_pin: SerialPin,
    dash_pin: SerialPin,
}

impl SerialPaddleInput {
    /// Open the named USB-serial device (as returned by [`available_ports`])
    /// and spawn the JNI polling thread. Blocks for up to ~3 s if the user
    /// has not yet granted USB permission for the device (the Java side
    /// surfaces the system permission dialog and waits for the user's choice).
    /// If the user doesn't grant in time, returns `Err` — Android remembers
    /// the choice, so re-selecting the port from the keyer settings panel
    /// after granting permission will succeed without re-prompting.
    pub fn open(port_name: &str, dit_pin: SerialPin, dash_pin: SerialPin) -> Result<Self, String> {
        let vm = JAVA_VM
            .get()
            .ok_or("JAVA_VM not initialised — call set_jvm() first")?;
        let ctx = CONTEXT
            .get()
            .ok_or("CONTEXT not initialised — call set_jvm() first")?;

        let mut env = vm
            .attach_current_thread()
            .map_err(|e| format!("attach_current_thread: {e}"))?;

        let helper_class = load_helper_class(&mut env)?;

        // UsbSerialHelper.openDevice(Context, String) — may block for up to
        // ~3 s while the system permission dialog is up (see the helper's
        // requestPermissionBlocking doc-comment for why 3 s, not 10 s).
        // If the user doesn't tap "Allow" in time, openDevice returns null
        // and we surface the failure; Android remembers the grant once
        // given, so a second invocation hits the fast path.
        let j_name = env
            .new_string(port_name)
            .map_err(|e| format!("new_string: {e}"))?;
        let helper_obj = env
            .call_static_method(
                &helper_class,
                "openDevice",
                "(Landroid/content/Context;Ljava/lang/String;)Lcom/radioutils/serial/UsbSerialHelper;",
                &[JValue::Object(ctx.as_obj()), JValue::Object(&j_name)],
            )
            .map_err(|e| {
                describe_and_clear_exception(&mut env, "openDevice");
                format!("openDevice: {e}")
            })?
            .l()
            .map_err(|e| format!("openDevice not object: {e}"))?;
        if helper_obj.is_null() {
            return Err(format!(
                "UsbSerialHelper.openDevice returned null for '{port_name}' \
                 (permission denied, device gone, or unsupported chip)"
            ));
        }

        // Resolve readModemStatus() once and reuse via call_method_unchecked.
        let helper_cls_local = env
            .get_object_class(&helper_obj)
            .map_err(|e| format!("get_object_class: {e}"))?;
        let read_mid: JMethodID = env
            .get_method_id(&helper_cls_local, "readModemStatus", "()I")
            .map_err(|e| format!("get_method_id readModemStatus: {e}"))?;

        let helper_ref = env
            .new_global_ref(&helper_obj)
            .map_err(|e| format!("new_global_ref helper: {e}"))?;

        let shared = Arc::new((
            Mutex::new(SharedState {
                dit: false,
                dash: false,
                timestamp: Instant::now(),
                generation: 0,
            }),
            Condvar::new(),
        ));
        let stop = Arc::new(AtomicBool::new(false));

        let poll_thread = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let helper_for_thread = helper_ref.clone();
            let mid = SendMid(read_mid.into_raw());
            std::thread::Builder::new()
                .name("usb-serial-poll".into())
                .spawn(move || {
                    poll_loop(helper_for_thread, mid, dit_pin, dash_pin, shared, stop);
                })
                .map_err(|e| format!("spawn poll thread: {e}"))?
        };

        Ok(Self {
            shared,
            helper_ref,
            stop,
            poll_thread: Some(poll_thread),
            port_name: port_name.to_string(),
            dit_pin,
            dash_pin,
        })
    }
}

// ---------------------------------------------------------------------------
// Poll loop
// ---------------------------------------------------------------------------

fn pin_mask(pin: SerialPin) -> i32 {
    // Matches UsbSerialHelper.CTS/DSR/DCD/RI.
    match pin {
        SerialPin::CTS => 1,
        SerialPin::DSR => 2,
        SerialPin::DCD => 4,
        SerialPin::RI => 8,
    }
}

fn poll_loop(
    helper_ref: GlobalRef,
    mid: SendMid,
    dit_pin: SerialPin,
    dash_pin: SerialPin,
    shared: Arc<(Mutex<SharedState>, Condvar)>,
    stop: Arc<AtomicBool>,
) {
    // Match the priority elevation used by the AMidi poll thread so the
    // scheduler treats this loop as audio-critical.
    crate::input::midi_android::set_thread_priority_audio();

    let Some(vm) = JAVA_VM.get() else {
        log::error!("[USB-Serial poll] no JVM; paddle input is now inactive");
        return;
    };

    // Attach as daemon — JVM teardown does not block on this thread.
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(e) => e,
        Err(e) => {
            log::error!(
                "[USB-Serial poll] attach_as_daemon failed: {e}; paddle input is now inactive"
            );
            return;
        }
    };

    let dit_mask = pin_mask(dit_pin);
    let dah_mask = pin_mask(dash_pin);
    // SAFETY: the JMethodID was resolved against the helper's class and the
    // helper object is kept alive by `helper_ref`. `jmethodID` is valid for
    // the lifetime of the loaded class.
    let mid = unsafe { JMethodID::from_raw(mid.0) };

    let mut consecutive_errors: u32 = 0;
    const MAX_ERRORS: u32 = 50;

    // Idle-backoff state: once the modem status has been stable for
    // IDLE_BACKOFF_THRESHOLD, slow the poll cadence to BACKOFF_SLEEP_MS
    // between iterations. Without this the vendor-chip path (FTDI / CH340 /
    // CP210x) hammers the USB bus at ~1 kHz indefinitely — fine for paddle
    // latency but rough on a phone battery during long idle stretches.
    // The threshold is small enough that pressing the paddle while idle has
    // worst-case latency = BACKOFF_SLEEP_MS + USB SOF ≈ 6 ms, well below
    // any human-perceptible delay. Any change resets the backoff.
    const IDLE_BACKOFF_THRESHOLD: Duration = Duration::from_millis(250);
    const BACKOFF_SLEEP_MS: u64 = 5;
    let mut last_change = Instant::now();

    log::debug!("[USB-Serial poll] thread started");

    while !stop.load(Ordering::Relaxed) {
        // SAFETY: `mid` is a valid jmethodID for `readModemStatus()` on the
        // class of `helper_ref`, signature `()I`.
        let res = unsafe {
            env.call_method_unchecked(
                helper_ref.as_obj(),
                mid,
                ReturnType::Primitive(Primitive::Int),
                &[],
            )
        };
        let status = match res.and_then(|v| v.i()) {
            Ok(v) => v,
            Err(e) => {
                describe_and_clear_exception(&mut env, "readModemStatus");
                consecutive_errors += 1;
                if consecutive_errors >= MAX_ERRORS {
                    log::error!(
                        "[USB-Serial poll] persistent JNI error: {e}; \
                         stopping (paddle input is now inactive)"
                    );
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
        };

        if status < 0 {
            // Transient device error (cable unplugged mid-transfer, etc.).
            consecutive_errors += 1;
            if consecutive_errors >= MAX_ERRORS {
                log::error!(
                    "[USB-Serial poll] persistent device error; \
                     stopping (paddle input is now inactive — re-plug the dongle)"
                );
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
            continue;
        }
        consecutive_errors = 0;

        let dit = (status & dit_mask) != 0;
        let dash = (status & dah_mask) != 0;

        let mut changed = false;
        let (lock, cvar) = &*shared;
        if let Ok(mut s) = lock.lock() {
            if s.dit != dit || s.dash != dash {
                s.dit = dit;
                s.dash = dash;
                s.timestamp = Instant::now();
                s.generation = s.generation.wrapping_add(1);
                cvar.notify_all();
                changed = true;
            }
        }

        if changed {
            last_change = Instant::now();
        } else if last_change.elapsed() >= IDLE_BACKOFF_THRESHOLD {
            // Stable for >250 ms — back off to spare the bus / battery.
            // CDC-ACM already parks the thread inside bulkTransfer for up to
            // 100 ms, so the extra 5 ms is negligible there; vendor chips
            // would otherwise spin at the USB SOF rate.
            std::thread::sleep(Duration::from_millis(BACKOFF_SLEEP_MS));
        }
        // While `last_change` is recent, the synchronous control transfer
        // inside readModemStatus() (~1 ms per iteration on full-speed
        // USB-OTG) is the natural rate limiter.
    }

    log::debug!("[USB-Serial poll] thread exiting");
}

/// Log the pending JNI exception's stack trace and then clear it. JNI
/// requires the exception to be cleared before any subsequent JNI call on
/// this thread, but a bare `exception_clear()` discards the cause; this
/// helper preserves it in logcat first.
fn describe_and_clear_exception(env: &mut JNIEnv, context: &str) {
    // `exception_describe` writes to stderr (which on Android is captured
    // into logcat under tag DEBUG). It is a no-op if no exception is pending.
    let _ = env.exception_describe();
    if let Err(e) = env.exception_clear() {
        log::warn!("[USB-Serial] exception_clear after {context} failed: {e}");
    }
}

// ---------------------------------------------------------------------------
// PaddleInput impl
// ---------------------------------------------------------------------------

impl PaddleInput for SerialPaddleInput {
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
        let s = lock.lock().unwrap();
        PaddleState {
            dit: s.dit,
            dash: s.dash,
            timestamp: s.timestamp,
        }
    }

    fn describe(&self) -> String {
        format_serial_description(&self.port_name, self.dit_pin, self.dash_pin)
    }
}

impl Drop for SerialPaddleInput {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.poll_thread.take() {
            let _ = handle.join();
        }
        // Close the Java helper (releases USB interface + closes connection).
        if let Some(vm) = JAVA_VM.get() {
            if let Ok(mut env) = vm.attach_current_thread() {
                if env
                    .call_method(self.helper_ref.as_obj(), "close", "()V", &[])
                    .is_err()
                {
                    describe_and_clear_exception(&mut env, "UsbSerialHelper.close");
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public free functions
// ---------------------------------------------------------------------------

/// Human-readable description of a USB-serial paddle configuration.
pub fn format_serial_description(port: &str, dit: SerialPin, dash: SerialPin) -> String {
    format!(
        "USB-serial paddle on {} (dit={:?}, dash={:?})",
        port, dit, dash
    )
}

/// Return the names of all currently-attached USB-serial devices we support.
/// Returns an empty `Vec` if the JVM/Context have not been initialised.
pub fn available_ports() -> Vec<String> {
    let Some(vm) = JAVA_VM.get() else {
        return Vec::new();
    };
    let Some(ctx) = CONTEXT.get() else {
        return Vec::new();
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return Vec::new();
    };

    let cls = match load_helper_class(&mut env) {
        Ok(c) => c,
        Err(e) => {
            log::warn!("[USB-Serial] load helper class for listing: {e}");
            return Vec::new();
        }
    };

    let arr_val = match env.call_static_method(
        &cls,
        "listDevices",
        "(Landroid/content/Context;)[Ljava/lang/String;",
        &[JValue::Object(ctx.as_obj())],
    ) {
        Ok(v) => v,
        Err(e) => {
            describe_and_clear_exception(&mut env, "listDevices");
            log::warn!("[USB-Serial] listDevices failed: {e}");
            return Vec::new();
        }
    };
    let arr_obj = match arr_val.l() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if arr_obj.is_null() {
        return Vec::new();
    }

    let arr = unsafe { jni::objects::JObjectArray::from_raw(arr_obj.into_raw()) };
    let len = env.get_array_length(&arr).unwrap_or(0);
    let mut out = Vec::with_capacity(len as usize);
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
            out.push(s.into());
        };
    }
    out
}

// ---------------------------------------------------------------------------
// Private: load UsbSerialHelper class from the embedded DEX
// ---------------------------------------------------------------------------

fn load_helper_class<'a>(env: &mut JNIEnv<'a>) -> Result<JClass<'a>, String> {
    if let Some(cached) = HELPER_CLASS.get() {
        let local = env
            .new_local_ref(cached.as_obj())
            .map_err(|e| format!("new_local_ref cached: {e}"))?;
        return Ok(unsafe { JClass::from_raw(local.into_raw()) });
    }

    // jbyteArray from RADIO_UTILS_KEYER_DEX
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

    // Tiny inline helper: `&mut env` doesn't work inside closures that
    // already borrow `env`, so we capture and clear by hand. Logs the
    // pending JNI exception's stack trace to logcat before clearing it,
    // which would otherwise be lost.
    macro_rules! describe_clear {
        ($env:expr) => {{
            let _ = $env.exception_describe();
            let _ = $env.exception_clear();
        }};
    }

    // ByteBuffer.wrap
    let byte_buffer = env
        .call_static_method(
            "java/nio/ByteBuffer",
            "wrap",
            "([B)Ljava/nio/ByteBuffer;",
            &[JValue::Object(&arr.into())],
        )
        .map_err(|e| {
            describe_clear!(env);
            format!("ByteBuffer.wrap: {e}")
        })?
        .l()
        .map_err(|e| format!("ByteBuffer not object: {e}"))?;

    // Parent classloader from current thread.
    let thread = env
        .call_static_method(
            "java/lang/Thread",
            "currentThread",
            "()Ljava/lang/Thread;",
            &[],
        )
        .map_err(|e| {
            describe_clear!(env);
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
            describe_clear!(env);
            format!("getContextClassLoader: {e}")
        })?
        .l()
        .map_err(|e| format!("classloader not object: {e}"))?;

    let loader = env
        .new_object(
            "dalvik/system/InMemoryDexClassLoader",
            "(Ljava/nio/ByteBuffer;Ljava/lang/ClassLoader;)V",
            &[JValue::Object(&byte_buffer), JValue::Object(&parent_cl)],
        )
        .map_err(|e| {
            describe_clear!(env);
            format!("InMemoryDexClassLoader: {e}")
        })?;

    let class_name = env
        .new_string("com.radioutils.serial.UsbSerialHelper")
        .map_err(|e| format!("new_string class name: {e}"))?;
    let class_obj = env
        .call_method(
            &loader,
            "loadClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&class_name)],
        )
        .map_err(|e| {
            describe_clear!(env);
            format!("loadClass: {e}")
        })?
        .l()
        .map_err(|e| format!("loadClass not object: {e}"))?;

    let global = env
        .new_global_ref(&class_obj)
        .map_err(|e| format!("global ref class: {e}"))?;
    HELPER_CLASS.set(global).ok();

    let cached = HELPER_CLASS
        .get()
        .expect("HELPER_CLASS was just initialised");
    let local = env
        .new_local_ref(cached.as_obj())
        .map_err(|e| format!("new_local_ref after set: {e}"))?;
    Ok(unsafe { JClass::from_raw(local.into_raw()) })
}
