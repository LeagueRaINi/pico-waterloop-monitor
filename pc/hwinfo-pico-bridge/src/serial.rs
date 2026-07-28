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
use std::io::{self, Read};
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

/// What went wrong talking to a port.
///
/// The distinction worth having a type for is `Busy` against `Absent`: telling
/// someone their Pico is unplugged when in fact Thonny — or the bridge itself —
/// simply has the port open is a much more annoying thing to be told.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "could not open {port}: {source} (another program has it open — \
         Thonny, a serial terminal, or the bridge itself)"
    )]
    Busy {
        port: String,
        source: serialport::Error,
    },

    #[error("could not open {port}: {source} (port does not exist)")]
    Absent {
        port: String,
        source: serialport::Error,
    },

    #[error("could not open {port}: {source}")]
    Open {
        port: String,
        source: serialport::Error,
    },

    #[error("write to {port} failed: {source}")]
    Write { port: String, source: io::Error },

    #[error("read from {port} failed: {source}")]
    Read { port: String, source: io::Error },
}

impl Error {
    pub fn is_busy(&self) -> bool {
        matches!(self, Error::Busy { .. })
    }
}

/// Everything upstream of `serial` reports failures as prose, so this is what
/// lets `?` carry one of these into a `Result<_, String>` unchanged.
impl From<Error> for String {
    fn from(err: Error) -> String {
        err.to_string()
    }
}

/// Work out which kind of open failure this was.
///
/// `serialport` folds "access denied" and "not found" into a single `NoDevice`
/// kind, and the only other thing it carries is the OS's own message — which
/// `FormatMessageW` returns in the user's language, so matching on its text
/// works on an English Windows and quietly stops working anywhere else. One
/// more enumeration settles it instead: a port that is still listed exists, so
/// the open was refused because something else holds it.
fn classify(name: &str, source: serialport::Error) -> Error {
    let port = name.to_string();
    if source.kind() != serialport::ErrorKind::NoDevice {
        return Error::Open { port, source };
    }
    if present(name) {
        Error::Busy { port, source }
    } else {
        Error::Absent { port, source }
    }
}

fn present(name: &str) -> bool {
    serialport::available_ports()
        .unwrap_or_default()
        .iter()
        .any(|p| p.port_name.eq_ignore_ascii_case(name))
}

pub struct Port {
    inner: Box<dyn SerialPort>,
    pub name: String,
}

impl Port {
    pub fn open(name: &str) -> Result<Port, Error> {
        Self::open_with_timeout(name, WRITE_TIMEOUT)
    }

    /// `timeout` bounds a single read or write, not a whole exchange. The
    /// updater wants it short so it can poll against its own deadline.
    pub fn open_with_timeout(name: &str, timeout: Duration) -> Result<Port, Error> {
        let inner = serialport::new(name, 115_200)
            .data_bits(DataBits::Eight)
            .parity(Parity::None)
            .stop_bits(StopBits::One)
            .flow_control(FlowControl::None)
            .timeout(timeout)
            .open()
            .map_err(|err| classify(name, err))?;

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

    pub fn write_bytes(&mut self, data: &[u8]) -> Result<(), Error> {
        self.inner
            .write_all(data)
            .and_then(|()| self.inner.flush())
            .map_err(|source| Error::Write {
                port: self.name.clone(),
                source,
            })
    }

    pub fn write_line(&mut self, line: &str) -> Result<(), Error> {
        self.write_bytes(line.as_bytes())
    }

    /// A timed-out read is reported as `Ok(0)`: with USB CDC it just means the
    /// device had nothing to say yet, which callers polling a deadline of their
    /// own do not want to treat as an error.
    pub fn read_some(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        match self.inner.read(buf) {
            Ok(n) => Ok(n),
            Err(err) if err.kind() == io::ErrorKind::TimedOut => Ok(0),
            Err(source) => Err(Error::Read {
                port: self.name.clone(),
                source,
            }),
        }
    }

    /// Throw away anything the device sent before now, so a reply cannot be
    /// confused with the tail of whatever it was doing beforehand.
    pub fn discard_input(&mut self) {
        let _ = self.inner.clear(serialport::ClearBuffer::Input);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_device(description: &str) -> serialport::Error {
        serialport::Error::new(serialport::ErrorKind::NoDevice, description)
    }

    #[test]
    fn a_busy_port_is_told_from_an_absent_one_without_reading_the_message() {
        // The regression this exists for: `FormatMessageW` returns the OS
        // message in the user's language, so matching it for "access is
        // denied" worked on an English Windows and quietly stopped working
        // anywhere else. Whatever the description says, a name that is not in
        // the enumeration belongs to no port.
        let err = classify("COM_NOT_A_REAL_PORT", no_device("Zugriff verweigert"));
        assert!(matches!(err, Error::Absent { .. }), "{err}");
        assert!(!err.is_busy());
    }

    #[test]
    fn a_failure_that_is_not_about_the_device_is_left_alone() {
        let source = serialport::Error::new(serialport::ErrorKind::InvalidInput, "bad baud");
        let err = classify("COM7", source);
        assert!(matches!(err, Error::Open { .. }), "{err}");
        assert!(!err.is_busy());
    }

    #[test]
    fn each_open_failure_says_what_to_do_about_it() {
        let busy = Error::Busy {
            port: "COM7".to_string(),
            source: no_device("Access is denied."),
        };
        assert!(busy.is_busy());
        assert!(
            busy.to_string().contains("another program has it open"),
            "{busy}"
        );

        let absent = Error::Absent {
            port: "COM7".to_string(),
            source: no_device("The system cannot find the file specified."),
        };
        assert!(!absent.is_busy());
        assert!(
            absent.to_string().contains("port does not exist"),
            "{absent}"
        );
    }
}
