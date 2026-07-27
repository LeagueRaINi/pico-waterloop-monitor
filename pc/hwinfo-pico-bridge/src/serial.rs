//! Finding and talking to the Pico's USB CDC port, on top of the `serialport`
//! crate.
//!
//! Baud rate is meaningless for USB CDC — the Pico ignores it — but the port
//! still has to be opened as a plain 8N1 binary stream.
//!
//! The bridge only ever writes; the firmware updater in `crate::pico` also
//! reads, because the raw REPL is a request/response protocol. Both go through
//! `Port` so `serialport` stays confined to this module.

use serialport::{
    DataBits, FlowControl, Parity, SerialPort, SerialPortInfo, SerialPortType, StopBits,
};
use std::io::Read;
use std::time::Duration;

/// Raspberry Pi Trading / Raspberry Pi Ltd. RP2040 and RP2350 both use this.
const PICO_VID: u16 = 0x2E8A;
/// The PID MicroPython's USB CDC enumerates as. Any 2E8A device is accepted,
/// but this one is preferred when several are plugged in.
const MICROPYTHON_PID: u16 = 0x0005;

/// Writes are small and the Pico drains them promptly; this only exists so a
/// device that has stopped reading cannot wedge the loop forever.
const WRITE_TIMEOUT: Duration = Duration::from_millis(1000);

pub struct PicoPort {
    pub name: String,
    /// Human-readable identification, shown on connect.
    pub device: String,
    is_micropython: bool,
}

fn describe(info: &SerialPortInfo) -> Option<PicoPort> {
    let SerialPortType::UsbPort(usb) = &info.port_type else {
        return None;
    };
    if usb.vid != PICO_VID {
        return None;
    }
    let mut device = format!("VID_{:04X}&PID_{:04X}", usb.vid, usb.pid);
    if let Some(product) = &usb.product {
        device.push_str(&format!(" ({product})"));
    }
    Some(PicoPort {
        name: info.port_name.clone(),
        device,
        is_micropython: usb.pid == MICROPYTHON_PID,
    })
}

fn pico_ports(ports: &[SerialPortInfo]) -> Vec<PicoPort> {
    let mut found: Vec<PicoPort> = ports.iter().filter_map(describe).collect();
    found.sort_by_key(|p| !p.is_micropython);
    found
}

/// Every currently-present COM port that belongs to a Raspberry Pi USB device,
/// MicroPython's PID first.
///
/// `available_ports` enumerates what is plugged in *now* and reports the USB
/// VID/PID for each, so a stale entry cannot point us at a COM number that has
/// since been reassigned to something else.
pub fn find_pico_ports() -> Vec<PicoPort> {
    pico_ports(&serialport::available_ports().unwrap_or_default())
}

pub enum PortCheck {
    /// Present, and it is a Raspberry Pi device.
    Pico(String),
    /// Present, but it belongs to something else — writing to it would be bad.
    /// Carries the Raspberry Pi ports that *are* present, for the error.
    NotPico(Vec<String>),
    /// Not plugged in at all.
    Absent,
}

/// One enumeration answers all three questions. It is a SetupAPI walk, so it
/// is worth not doing it three times over.
pub fn check_port(name: &str) -> PortCheck {
    let ports = serialport::available_ports().unwrap_or_default();
    let picos = pico_ports(&ports);

    if let Some(p) = picos.iter().find(|p| p.name.eq_ignore_ascii_case(name)) {
        return PortCheck::Pico(p.device.clone());
    }
    if ports.iter().any(|p| p.port_name.eq_ignore_ascii_case(name)) {
        return PortCheck::NotPico(picos.into_iter().map(|p| p.name).collect());
    }
    PortCheck::Absent
}

/// Whether an open failed because something else holds the port.
///
/// This goes by the message rather than `err.kind()` on purpose: `serialport`
/// reports a Windows sharing violation as `NoDevice`, so trusting the kind
/// tells someone their Pico is unplugged when in fact the bridge — or Thonny —
/// simply has it open, which is a much more annoying thing to be told.
pub fn is_busy(message: &str) -> bool {
    message.to_lowercase().contains("access is denied")
}

pub struct Port {
    inner: Box<dyn SerialPort>,
    pub name: String,
}

impl Port {
    pub fn open(name: &str) -> Result<Port, String> {
        Self::open_with_timeout(name, WRITE_TIMEOUT)
    }

    /// `timeout` bounds a single read or write, not a whole exchange. The
    /// updater wants it short so it can poll against its own deadline.
    pub fn open_with_timeout(name: &str, timeout: Duration) -> Result<Port, String> {
        let inner = serialport::new(name, 115_200)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(timeout)
            .open()
            .map_err(|err| {
                let text = err.to_string();
                let hint = if is_busy(&text) {
                    " (another program has it open — Thonny, a serial terminal, \
                      or the bridge itself)"
                } else if err.kind() == serialport::ErrorKind::NoDevice {
                    " (port does not exist)"
                } else {
                    ""
                };
                format!("could not open {name}: {text}{hint}")
            })?;

        let mut port = Port {
            inner,
            name: name.to_string(),
        };
        // Asserting DTR/RTS is what makes the CDC host side consider the link
        // open. Not fatal if the driver refuses.
        let _ = port.inner.write_data_terminal_ready(true);
        let _ = port.inner.write_request_to_send(true);
        Ok(port)
    }

    pub fn write_bytes(&mut self, data: &[u8]) -> Result<(), String> {
        self.inner
            .write_all(data)
            .and_then(|()| self.inner.flush())
            .map_err(|err| format!("write to {} failed: {err}", self.name))
    }

    pub fn write_line(&mut self, line: &str) -> Result<(), String> {
        self.write_bytes(line.as_bytes())
    }

    /// A timed-out read is reported as `Ok(0)`: with USB CDC it just means the
    /// device had nothing to say yet, which callers polling a deadline of their
    /// own do not want to treat as an error.
    pub fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, String> {
        match self.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == std::io::ErrorKind::TimedOut => Ok(0),
            Err(err) => Err(format!("read from {} failed: {err}", self.name)),
        }
    }

    /// Throw away anything the device sent before now, so a reply cannot be
    /// confused with the tail of whatever it was doing beforehand.
    pub fn discard_input(&mut self) {
        let _ = self.inner.clear(serialport::ClearBuffer::Input);
    }
}
