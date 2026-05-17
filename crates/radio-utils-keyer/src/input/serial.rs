use std::time::{Duration, Instant};

use serialport::SerialPort;

use super::{PaddleInput, PaddleState};
use crate::config::SerialPin;

/// Build a human-readable description of the serial paddle configuration.
pub fn format_serial_description(port: &str, dit: SerialPin, dash: SerialPin) -> String {
    format!("Serial paddle on {} (dit={:?}, dash={:?})", port, dit, dash)
}

/// Read the logical state of a single serial control pin.
pub fn read_pin(port: &mut dyn SerialPort, pin: SerialPin) -> bool {
    match pin {
        SerialPin::CTS => port.read_clear_to_send().unwrap_or(false),
        SerialPin::DSR => port.read_data_set_ready().unwrap_or(false),
        SerialPin::DCD => port.read_carrier_detect().unwrap_or(false),
        SerialPin::RI => port.read_ring_indicator().unwrap_or(false),
    }
}

/// Serial-port based paddle input using modem control pins.
pub struct SerialPaddleInput {
    port: Box<dyn SerialPort>,
    dit_pin: SerialPin,
    dash_pin: SerialPin,
    state: PaddleState,
    port_name: String,
}

impl SerialPaddleInput {
    /// Open the named serial port at 9600 baud with a 1 ms timeout.
    pub fn open(
        port_name: &str,
        dit_pin: SerialPin,
        dash_pin: SerialPin,
    ) -> Result<Self, serialport::Error> {
        let port = serialport::new(port_name, 9600)
            .timeout(Duration::from_millis(1))
            .open()?;

        Ok(Self {
            port,
            dit_pin,
            dash_pin,
            state: PaddleState::default(),
            port_name: port_name.to_string(),
        })
    }

    /// Sample the current pin values and return the resulting [`PaddleState`].
    fn sample(&mut self) -> PaddleState {
        PaddleState {
            dit: read_pin(self.port.as_mut(), self.dit_pin),
            dash: read_pin(self.port.as_mut(), self.dash_pin),
            timestamp: Instant::now(),
        }
    }

    /// Attempt to use `TIOCMIWAIT` ioctl on Linux to block until a modem-status
    /// line changes. Returns `Err` on non-Linux platforms or if the ioctl fails.
    #[cfg(target_os = "linux")]
    fn tiocmiwait(&self) -> Result<(), std::io::Error> {
        // TIOCMIWAIT requires the raw file descriptor from the serial port.
        // The serialport crate's concrete TTYPort type implements AsRawFd,
        // but is not easily accessible through the trait object. For now,
        // return Err so the caller falls back to polling.
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "TIOCMIWAIT not yet wired up",
        ))
    }

    #[cfg(not(target_os = "linux"))]
    fn tiocmiwait(&self) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "TIOCMIWAIT is only available on Linux",
        ))
    }
}

impl PaddleInput for SerialPaddleInput {
    fn wait_for_change(&mut self, timeout: Option<Duration>) -> PaddleState {
        let old = self.state;

        // First, try the efficient kernel-level wait.
        if self.tiocmiwait().is_ok() {
            self.state = self.sample();
            return self.state;
        }

        // Fallback: poll at 1 ms intervals.
        let deadline = timeout.map(|d| Instant::now() + d);

        loop {
            let current = self.sample();
            if current.dit != old.dit || current.dash != old.dash {
                self.state = current;
                return self.state;
            }
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    // Timeout expired — return current (unchanged) state.
                    self.state = current;
                    return self.state;
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn read(&self) -> PaddleState {
        // Return the last known cached state. The state is updated by
        // `wait_for_change`, which is the primary polling loop. We cannot
        // call the serial port methods here because they require `&mut self`.
        self.state
    }

    fn describe(&self) -> String {
        format_serial_description(&self.port_name, self.dit_pin, self.dash_pin)
    }
}

/// List the names of all available serial ports on this system.
pub fn available_ports() -> Vec<String> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.port_name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn port_config_description() {
        let desc = format_serial_description("/dev/ttyUSB0", SerialPin::CTS, SerialPin::DSR);
        assert_eq!(desc, "Serial paddle on /dev/ttyUSB0 (dit=CTS, dash=DSR)");

        let desc2 = format_serial_description("COM3", SerialPin::DCD, SerialPin::RI);
        assert_eq!(desc2, "Serial paddle on COM3 (dit=DCD, dash=RI)");
    }

    #[test]
    fn available_ports_returns_vec() {
        // Just verify it returns a Vec<String> without panicking.
        let ports: Vec<String> = available_ports();
        // We cannot assert specific ports in CI, but the call must not panic.
        let _ = ports;
    }
}
