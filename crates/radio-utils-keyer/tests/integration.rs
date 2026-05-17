use radio_utils_keyer::input::keyboard::KeyboardPaddleInput;
use radio_utils_keyer::*;
use std::time::Duration;

#[test]
fn full_keyer_cycle_keyboard_iambic_b() {
    let config = KeyerConfig {
        speed_wpm: 30, // fast for quicker test
        mode: KeyerMode::IambicB,
        hang_time_ms: 100,
        ..Default::default()
    };

    let (sender, kb_input) = KeyboardPaddleInput::new();
    let (mut handle, output_rx) = KeyerHandle::start(config, Box::new(kb_input));

    // Let the keyer thread enter its first wait_for_change before sending
    // events — otherwise both DitDown and DitUp can land in the same
    // pre-startup batch, where `drain_events` collapses them to a final
    // state of (false, false) and the engine never sees the rising edge.
    std::thread::sleep(Duration::from_millis(50));

    // Hold the dit longer than the keyer thread's 100 ms idle-wait
    // timeout. Once the engine is active the poll drops to 1 ms, so a
    // short release is fine — but the *initial* press must outlast one
    // idle quantum for the dit to register on slow CI runners.
    sender.send(PaddleEvent::DitDown);
    std::thread::sleep(Duration::from_millis(150));
    sender.send(PaddleEvent::DitUp);

    // 30 WPM dit = 40 ms, hang_time = 100 ms, so PttRequest(false)
    // arrives ~140 ms after press. Give 800 ms for headroom against
    // thread-starvation lag on macOS/Windows CI runners (the tick
    // batch is capped at 4 ms, so real-time ticks get dropped under
    // load).
    std::thread::sleep(Duration::from_millis(800));
    let mut outputs = Vec::new();
    while let Ok(out) = output_rx.try_recv() {
        outputs.push(out);
    }

    // Should have: PttRequest(true), KeyDown, KeyUp, PttRequest(false)
    assert!(
        outputs.contains(&KeyerOutput::PttRequest(true)),
        "missing PttRequest(true) in {:?}",
        outputs
    );
    assert!(
        outputs.contains(&KeyerOutput::KeyDown),
        "missing KeyDown in {:?}",
        outputs
    );
    assert!(
        outputs.contains(&KeyerOutput::KeyUp),
        "missing KeyUp in {:?}",
        outputs
    );
    assert!(
        outputs.contains(&KeyerOutput::PttRequest(false)),
        "missing PttRequest(false) in {:?}",
        outputs
    );

    handle.stop();
}

#[test]
fn macro_sends_and_completes() {
    let config = KeyerConfig {
        speed_wpm: 40,
        mode: KeyerMode::IambicB,
        hang_time_ms: 50,
        ..Default::default()
    };

    let (_sender, kb_input) = KeyboardPaddleInput::new();
    let (mut handle, output_rx) = KeyerHandle::start(config, Box::new(kb_input));

    handle.send_macro("E".to_string()); // E = single dit

    std::thread::sleep(Duration::from_millis(200));
    let mut outputs = Vec::new();
    while let Ok(out) = output_rx.try_recv() {
        outputs.push(out);
    }

    assert!(
        outputs.contains(&KeyerOutput::KeyDown),
        "missing KeyDown from macro in {:?}",
        outputs
    );
    assert!(
        outputs.contains(&KeyerOutput::KeyUp),
        "missing KeyUp from macro in {:?}",
        outputs
    );

    handle.stop();
}

#[test]
fn config_update_changes_speed() {
    let config = KeyerConfig {
        speed_wpm: 20,
        mode: KeyerMode::IambicB,
        hang_time_ms: 50,
        ..Default::default()
    };

    let (_sender, kb_input) = KeyboardPaddleInput::new();
    let (mut handle, _output_rx) = KeyerHandle::start(config, Box::new(kb_input));

    // Update speed -- should not panic or deadlock
    let new_config = KeyerConfig {
        speed_wpm: 35,
        ..Default::default()
    };
    handle.update_config(new_config);

    std::thread::sleep(Duration::from_millis(50));

    handle.stop();
}

#[cfg(not(target_arch = "wasm32"))]
#[test]
fn midi_input_device_list_returns_vec() {
    // Smoke test — just check it doesn't panic even if no MIDI devices are present
    let devices = radio_utils_keyer::list_midi_input_devices();
    let _ = devices; // can be empty on CI
}
