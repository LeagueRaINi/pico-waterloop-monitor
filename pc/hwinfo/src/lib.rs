//! Reader for HWiNFO's "Shared Memory Support" block (HWiNFO_SENS_SM2).
//!
//! There is no mature crate for this — HWiNFO exposes a raw memory layout, not
//! an API — so the struct walk is done here, over a mapping obtained through
//! Microsoft's own `windows-sys` bindings.
//!
//! The published SDK headers show the structures with default MSVC alignment,
//! but the block HWiNFO actually publishes is packed, and recent builds append
//! UTF-8 copies of the strings after the fixed fields. So: stride by the
//! element sizes the header reports, and only decode the fixed prefix.
//!
//! Verified against HWiNFO64 shared memory version 2, revision 1 (sensor
//! element 392 bytes, reading element 460 bytes).
//!
//! `SharedMem` only knows how to get the bytes off Windows; everything that
//! makes sense of them lives in `parse`, as free functions over a plain
//! `&[u8]`. That split is what lets every reading HWiNFO publishes — not just
//! the ones a particular caller happens to want today — come out fully typed,
//! and lets the parsing be tested against a byte buffer built by hand, with
//! no HWiNFO instance and no shared-memory mapping required.

use std::ffi::c_void;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
    MEMORY_BASIC_INFORMATION, MEM_COMMIT,
};

const SHM_NAME: &str = "Global\\HWiNFO_SENS_SM2";
const SIGNATURE: [u8; 4] = *b"HWiS";

const STRING_LEN: usize = 128;
const UNIT_LEN: usize = 16;

/// What a reading measures — HWiNFO's `SENSOR_READING_TYPE`.
///
/// This is the complete set HWiNFO defines as of shared memory version 2,
/// revision 1, so a caller can match on `Voltage` or `Usage` today rather than
/// waiting for this crate to grow a variant for it. `Other` only ever carries
/// a code this crate has never seen — a future HWiNFO adding a tenth type, not
/// one of these eight.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ReadingKind {
    /// `SENSOR_TYPE_NONE`. Not expected on a real reading; HWiNFO's own
    /// placeholder for "unset".
    None,
    Temperature,
    Voltage,
    Fan,
    Current,
    Power,
    Clock,
    /// A percentage load, e.g. per-core CPU usage.
    Usage,
    /// A type code this crate does not have a name for — either HWiNFO's own
    /// catch-all (`SENSOR_TYPE_OTHER`, raw code 8) or, should HWiNFO ever
    /// define a ninth type, that.
    Other(u32),
}

impl ReadingKind {
    /// The type codes as HWiNFO writes them into the `SENSOR_READING_TYPE`
    /// field. A data-carrying variant rules out `#[repr(u32)]` discriminants,
    /// so the mapping is spelled out here.
    fn from_raw(raw: u32) -> ReadingKind {
        match raw {
            0 => ReadingKind::None,
            1 => ReadingKind::Temperature,
            2 => ReadingKind::Voltage,
            3 => ReadingKind::Fan,
            4 => ReadingKind::Current,
            5 => ReadingKind::Power,
            6 => ReadingKind::Clock,
            7 => ReadingKind::Usage,
            other => ReadingKind::Other(other),
        }
    }
}

impl std::fmt::Display for ReadingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadingKind::None => f.write_str("none"),
            ReadingKind::Temperature => f.write_str("temperature"),
            ReadingKind::Voltage => f.write_str("voltage"),
            ReadingKind::Fan => f.write_str("fan"),
            ReadingKind::Current => f.write_str("current"),
            ReadingKind::Power => f.write_str("power"),
            ReadingKind::Clock => f.write_str("clock"),
            ReadingKind::Usage => f.write_str("usage"),
            ReadingKind::Other(raw) => write!(f, "other({raw})"),
        }
    }
}

/// The three structures HWiNFO publishes, as the SDK header describes them.
///
/// Every field is byte-aligned — the integers are `zerocopy`'s little-endian
/// wrappers and the strings are byte arrays — so `repr(C)` lays these out with
/// no padding, matching the packed block on the wire without needing
/// `repr(packed)` and the unaligned-reference problems that come with it.
/// Little-endian is stated rather than assumed, which is also what the previous
/// `from_le_bytes` calls did.
mod layout {
    use zerocopy::byteorder::little_endian::{F64, I64, U32};
    use zerocopy::{FromBytes, Immutable, KnownLayout};

    #[derive(FromBytes, KnownLayout, Immutable)]
    #[repr(C)]
    pub struct Header {
        pub signature: [u8; 4],
        pub version: U32,
        pub revision: U32,
        pub poll_time: I64,
        pub offset_sensors: U32,
        pub size_sensor: U32,
        pub num_sensors: U32,
        pub offset_readings: U32,
        pub size_reading: U32,
        pub num_readings: U32,
    }

    /// Only the fixed prefix. Recent HWiNFO builds append UTF-8 copies of the
    /// strings after it, which is why the walk strides by the size the header
    /// reports rather than by `size_of` this.
    #[derive(FromBytes, KnownLayout, Immutable)]
    #[repr(C)]
    pub struct Sensor {
        pub id: U32,
        pub instance: U32,
        pub name_orig: [u8; super::STRING_LEN],
        pub name_user: [u8; super::STRING_LEN],
    }

    #[derive(FromBytes, KnownLayout, Immutable)]
    #[repr(C)]
    pub struct Reading {
        pub kind: U32,
        pub sensor_index: U32,
        pub id: U32,
        pub label_orig: [u8; super::STRING_LEN],
        pub label_user: [u8; super::STRING_LEN],
        pub unit: [u8; super::UNIT_LEN],
        pub value: F64,
        pub value_min: F64,
        pub value_max: F64,
        pub value_avg: F64,
    }
}

use layout::{Header, Sensor};

/// The sizes the header must report at least, or this is not a layout we know.
const HEADER_SIZE: usize = std::mem::size_of::<Header>();
const SENSOR_FIXED: usize = std::mem::size_of::<Sensor>();
const READING_FIXED: usize = std::mem::size_of::<layout::Reading>();

/// A sensor — a physical or virtual device readings are grouped under, e.g.
/// "CPU [#0]: AMD Ryzen 7 7700X" or "GPU [#1]: NVIDIA GeForce RTX 4080".
///
/// `id`/`instance` are what HWiNFO itself uses to tell two sensors of the same
/// kind apart, and are stable across a rescan the way a table position is not
/// — a future caller distinguishing "the second GPU" from "the first" should
/// key off `instance` rather than where either currently sits in
/// [`SharedMem::sensors`].
#[derive(Clone, Debug)]
pub struct SensorInfo {
    pub id: u32,
    pub instance: u32,
    pub name: String,
}

/// One value HWiNFO is reporting, fully decoded.
///
/// Every field the wire format carries is here — not just the ones the pico
/// bridge happens to plot — so parsing a reading this crate has not been
/// taught about yet is never the blocker: `kind` already tells you what it
/// is, `value_min`/`value_max`/`value_avg` are already there, and `sensor_id`/
/// `sensor_instance` are already resolved. A future caller only has to read
/// the fields it wants.
#[derive(Clone, Debug)]
pub struct Reading {
    /// HWiNFO's own id for this reading.
    pub id: u32,
    /// The owning sensor's name, already resolved to whichever of the
    /// original or user-renamed copy HWiNFO prefers — see [`SensorInfo`] for
    /// the id/instance behind it.
    pub sensor: String,
    pub sensor_id: u32,
    pub sensor_instance: u32,
    pub label: String,
    pub unit: String,
    pub value: f64,
    pub value_min: f64,
    pub value_max: f64,
    pub value_avg: f64,
    pub kind: ReadingKind,
}

/// HWiNFO lets the user rename anything; the original is what it shipped with.
fn preferred(user: &[u8], original: &[u8]) -> String {
    let user = latin1(user);
    if user.is_empty() {
        latin1(original)
    } else {
        user
    }
}

/// HWiNFO writes these as single-byte characters (0xB0 for the degree sign),
/// i.e. Latin-1 rather than UTF-8 — which is exactly what casting each byte to
/// `char` decodes.
fn latin1(raw: &[u8]) -> String {
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    raw[..end]
        .iter()
        .map(|&b| b as char)
        .collect::<String>()
        .trim()
        .to_string()
}

/// Parsing over a plain byte slice — no shared memory, no Windows API, no
/// `SharedMem` required. `SharedMem`'s methods are thin wrappers over these;
/// tests build the slice by hand instead of a live HWiNFO instance.
mod parse {
    use super::{
        latin1, layout, preferred, Header, Reading, ReadingKind, Sensor, SensorInfo, READING_FIXED,
        SENSOR_FIXED, SIGNATURE,
    };
    use zerocopy::{FromBytes, Immutable, KnownLayout};

    fn struct_at<T: FromBytes + KnownLayout + Immutable>(
        bytes: &[u8],
        offset: usize,
    ) -> Option<&T> {
        let end = offset.checked_add(std::mem::size_of::<T>())?;
        T::ref_from_prefix(bytes.get(offset..end)?)
            .ok()
            .map(|(value, _rest)| value)
    }

    pub(crate) fn header(bytes: &[u8]) -> Option<&Header> {
        struct_at::<Header>(bytes, 0)
    }

    /// The header, but only once it looks like a live HWiNFO instance wrote
    /// it — every other function here is built on top of this one.
    pub(crate) fn header_checked(bytes: &[u8]) -> Result<&Header, String> {
        header(bytes)
            .filter(|h| h.signature == SIGNATURE)
            .ok_or_else(|| "HWiNFO shared memory went away".to_string())
    }

    pub(crate) fn sensors(bytes: &[u8], header: &Header) -> Result<Vec<SensorInfo>, String> {
        let off_sensors = header.offset_sensors.get() as usize;
        let size_sensor = header.size_sensor.get() as usize;
        if size_sensor < SENSOR_FIXED {
            return Err(format!(
                "unexpected HWiNFO layout (sensor {size_sensor} B) — this build \
                 of HWiNFO is not supported"
            ));
        }

        // The count is a 32-bit field read out of a block another process is
        // writing, so it can be caught mid-update or simply be nonsense.
        // Believing it would mean reserving up to 4 G elements: an allocation
        // failure, which aborts the process outright under `panic = "abort"`.
        // Cap it at what the slice could physically hold — the walk below is
        // bounds checked anyway, so this only has to be sane.
        let fits = bytes.len().saturating_sub(off_sensors) / size_sensor.max(1);
        let num_sensors = (header.num_sensors.get() as usize).min(fits);

        let mut out = Vec::with_capacity(num_sensors);
        for i in 0..num_sensors {
            let Some(sensor) = struct_at::<Sensor>(bytes, off_sensors + i * size_sensor) else {
                break;
            };
            out.push(SensorInfo {
                id: sensor.id.get(),
                instance: sensor.instance.get(),
                name: preferred(&sensor.name_user, &sensor.name_orig),
            });
        }
        Ok(out)
    }

    pub(crate) fn read_all(bytes: &[u8]) -> Result<Vec<Reading>, String> {
        let header = header_checked(bytes)?;
        let sensors = sensors(bytes, header)?;

        let off_readings = header.offset_readings.get() as usize;
        let size_reading = header.size_reading.get() as usize;
        if size_reading < READING_FIXED {
            return Err(format!(
                "unexpected HWiNFO layout (reading {size_reading} B) — this build \
                 of HWiNFO is not supported"
            ));
        }

        let fits = bytes.len().saturating_sub(off_readings) / size_reading.max(1);
        let num_readings = (header.num_readings.get() as usize).min(fits);

        let mut out = Vec::with_capacity(num_readings);
        for i in 0..num_readings {
            let Some(raw) = struct_at::<layout::Reading>(bytes, off_readings + i * size_reading)
            else {
                break;
            };
            let sensor = sensors.get(raw.sensor_index.get() as usize);
            out.push(Reading {
                id: raw.id.get(),
                sensor: sensor.map(|s| s.name.clone()).unwrap_or_default(),
                sensor_id: sensor.map_or(0, |s| s.id),
                sensor_instance: sensor.map_or(0, |s| s.instance),
                label: preferred(&raw.label_user, &raw.label_orig),
                unit: latin1(&raw.unit),
                value: raw.value.get(),
                value_min: raw.value_min.get(),
                value_max: raw.value_max.get(),
                value_avg: raw.value_avg.get(),
                kind: ReadingKind::from_raw(raw.kind.get()),
            });
        }
        Ok(out)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub struct SharedMem {
    mapping: HANDLE,
    base: *const u8,
    len: usize,
}

impl SharedMem {
    pub fn open() -> Result<SharedMem, String> {
        let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, wide(SHM_NAME).as_ptr()) };
        if mapping.is_null() {
            return Err(format!(
                "HWiNFO shared memory not found (error {}). Is HWiNFO running with \
                 Settings -> Main Settings -> 'Shared Memory Support' enabled?",
                unsafe { GetLastError() }
            ));
        }

        let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            let err = unsafe { GetLastError() };
            unsafe { CloseHandle(mapping) };
            return Err(format!("could not map HWiNFO shared memory (error {err})"));
        }

        // Ask the OS how big the view actually is, so every read can be bounds
        // checked against something we did not get from the block itself.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let queried = unsafe {
            VirtualQuery(
                view.Value,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        let len = if queried != 0 && info.State == MEM_COMMIT {
            info.RegionSize
        } else {
            0
        };

        let shm = SharedMem {
            mapping,
            base: view.Value as *const u8,
            len,
        };
        if len < HEADER_SIZE {
            return Err("HWiNFO shared memory view is too small".into());
        }
        if !shm.is_valid() {
            return Err("HWiNFO shared memory is not ready yet".into());
        }
        Ok(shm)
    }

    /// The mapped view, as `parse` sees it. Its own length is the bound every
    /// walk below is checked against, so nothing here has to track `len`
    /// separately from the slice.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `base`/`len` describe the mapped, committed view for as
        // long as `self` is alive; nothing else in this process writes to it.
        unsafe { std::slice::from_raw_parts(self.base, self.len) }
    }

    /// True while the block still belongs to a live HWiNFO instance. It flips
    /// to "DEAD" on shutdown, and a restarted HWiNFO creates a *new* section,
    /// leaving ours frozen — which the caller detects via `poll_time`.
    pub fn is_valid(&self) -> bool {
        parse::header(self.as_bytes()).is_some_and(|header| header.signature == SIGNATURE)
    }

    pub fn poll_time(&self) -> i64 {
        parse::header(self.as_bytes()).map_or(0, |header| header.poll_time.get())
    }

    /// Every sensor HWiNFO is publishing, with no readings attached. Cheaper
    /// than [`SharedMem::read_all`] for a caller that only wants, say, to list
    /// what devices are present, or to resolve `sensor_id`/`sensor_instance`
    /// without decoding every reading too.
    pub fn sensors(&self) -> Result<Vec<SensorInfo>, String> {
        let bytes = self.as_bytes();
        parse::sensors(bytes, parse::header_checked(bytes)?)
    }

    pub fn read_all(&self) -> Result<Vec<Reading>, String> {
        parse::read_all(self.as_bytes())
    }
}

impl Drop for SharedMem {
    fn drop(&mut self) {
        unsafe {
            if !self.base.is_null() {
                UnmapViewOfFile(
                    windows_sys::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS {
                        Value: self.base as *mut c_void,
                    },
                );
            }
            if !self.mapping.is_null() {
                CloseHandle(self.mapping);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// The offsets the struct definitions replaced, kept as a test.
    ///
    /// Nothing at runtime would notice a field of the wrong width: the walk
    /// would simply read the next one's bytes and report a plausible-looking
    /// wrong number. These are the values verified against HWiNFO64 shared
    /// memory version 2, revision 1.
    #[test]
    fn the_header_is_laid_out_where_hwinfo_puts_it() {
        assert_eq!(size_of::<Header>(), 44);
        assert_eq!(offset_of!(Header, signature), 0);
        assert_eq!(offset_of!(Header, poll_time), 12);
        assert_eq!(offset_of!(Header, offset_sensors), 20);
        assert_eq!(offset_of!(Header, size_sensor), 24);
        assert_eq!(offset_of!(Header, num_sensors), 28);
        assert_eq!(offset_of!(Header, offset_readings), 32);
        assert_eq!(offset_of!(Header, size_reading), 36);
        assert_eq!(offset_of!(Header, num_readings), 40);
    }

    #[test]
    fn a_sensor_element_is_laid_out_where_hwinfo_puts_it() {
        assert_eq!(size_of::<Sensor>(), 264, "fixed prefix");
        assert_eq!(offset_of!(Sensor, id), 0);
        assert_eq!(offset_of!(Sensor, instance), 4);
        assert_eq!(offset_of!(Sensor, name_orig), 8);
        assert_eq!(offset_of!(Sensor, name_user), 136);
    }

    #[test]
    fn a_reading_element_is_laid_out_where_hwinfo_puts_it() {
        assert_eq!(size_of::<layout::Reading>(), 316, "fixed prefix");
        assert_eq!(offset_of!(layout::Reading, kind), 0);
        assert_eq!(offset_of!(layout::Reading, sensor_index), 4);
        assert_eq!(offset_of!(layout::Reading, id), 8);
        assert_eq!(offset_of!(layout::Reading, label_orig), 12);
        assert_eq!(offset_of!(layout::Reading, label_user), 140);
        assert_eq!(offset_of!(layout::Reading, unit), 268);
        assert_eq!(offset_of!(layout::Reading, value), 284);
        assert_eq!(offset_of!(layout::Reading, value_min), 292);
        assert_eq!(offset_of!(layout::Reading, value_max), 300);
        assert_eq!(offset_of!(layout::Reading, value_avg), 308);
    }

    #[test]
    fn the_reading_type_codes_are_the_ones_hwinfo_publishes() {
        // These used to be named constants; the match is now the only place
        // the numbers live, so this is what pins them.
        assert_eq!(ReadingKind::from_raw(0), ReadingKind::None);
        assert_eq!(ReadingKind::from_raw(1), ReadingKind::Temperature);
        assert_eq!(ReadingKind::from_raw(2), ReadingKind::Voltage);
        assert_eq!(ReadingKind::from_raw(3), ReadingKind::Fan);
        assert_eq!(ReadingKind::from_raw(4), ReadingKind::Current);
        assert_eq!(ReadingKind::from_raw(5), ReadingKind::Power);
        assert_eq!(ReadingKind::from_raw(6), ReadingKind::Clock);
        assert_eq!(ReadingKind::from_raw(7), ReadingKind::Usage);
        // 8 is HWiNFO's own "other" type; anything past it is simply a code
        // this crate has never seen. Both arrive the same way, so neither is
        // ever mistaken for one of the eight named types.
        assert_eq!(ReadingKind::from_raw(8), ReadingKind::Other(8));
        assert_eq!(ReadingKind::from_raw(99), ReadingKind::Other(99));
    }

    #[test]
    fn reading_kind_displays_as_a_lowercase_word() {
        assert_eq!(ReadingKind::Voltage.to_string(), "voltage");
        assert_eq!(ReadingKind::Other(42).to_string(), "other(42)");
    }

    #[test]
    fn strings_are_decoded_as_latin1_and_stop_at_the_terminator() {
        // 0xB0 is the degree sign HWiNFO puts in a temperature's unit. As UTF-8
        // that byte is not a character at all.
        assert_eq!(latin1(b"\xb0C\0junk after the terminator"), "°C");
        assert_eq!(latin1(b"  CPU Package  \0"), "CPU Package", "trimmed");
        assert_eq!(latin1(b"\0"), "");
        assert_eq!(latin1(b"no terminator"), "no terminator");
    }

    #[test]
    fn a_renamed_sensor_wins_over_the_original() {
        assert_eq!(
            preferred(b"Water Loop\0", b"Nuvoton NCT6798D\0"),
            "Water Loop"
        );
        assert_eq!(preferred(b"\0", b"Nuvoton NCT6798D\0"), "Nuvoton NCT6798D");
        // A name of nothing but spaces trims to empty, so it is not a rename.
        assert_eq!(preferred(b"   \0", b"Original\0"), "Original");
    }

    /// Builds a synthetic HWiNFO block byte for byte: header, then sensors,
    /// then readings, each packed at exactly the fixed size `layout`
    /// describes — `parse` only ever promises to decode that prefix, so a
    /// real (longer, UTF-8-suffixed) element must still work, and this is the
    /// shorter case.
    struct Fixture {
        bytes: Vec<u8>,
        sensors: u32,
        readings: u32,
    }

    impl Fixture {
        fn new() -> Fixture {
            let mut bytes = vec![0u8; size_of::<Header>()];
            bytes[0..4].copy_from_slice(&SIGNATURE);
            Fixture {
                bytes,
                sensors: 0,
                readings: 0,
            }
        }

        fn sensor(mut self, id: u32, instance: u32, name: &str) -> Fixture {
            let mut row = vec![0u8; size_of::<Sensor>()];
            row[0..4].copy_from_slice(&id.to_le_bytes());
            row[4..8].copy_from_slice(&instance.to_le_bytes());
            let name = name.as_bytes();
            row[136..136 + name.len()].copy_from_slice(name); // name_user
            self.bytes.extend_from_slice(&row);
            self.sensors += 1;
            self
        }

        #[allow(clippy::too_many_arguments)]
        fn reading(
            mut self,
            kind: u32,
            sensor_index: u32,
            id: u32,
            label: &str,
            unit: &[u8],
            value: f64,
            min: f64,
            max: f64,
            avg: f64,
        ) -> Fixture {
            let mut row = vec![0u8; size_of::<layout::Reading>()];
            row[0..4].copy_from_slice(&kind.to_le_bytes());
            row[4..8].copy_from_slice(&sensor_index.to_le_bytes());
            row[8..12].copy_from_slice(&id.to_le_bytes());
            let label = label.as_bytes();
            row[140..140 + label.len()].copy_from_slice(label); // label_user
            row[268..268 + unit.len()].copy_from_slice(unit);
            row[284..292].copy_from_slice(&value.to_le_bytes());
            row[292..300].copy_from_slice(&min.to_le_bytes());
            row[300..308].copy_from_slice(&max.to_le_bytes());
            row[308..316].copy_from_slice(&avg.to_le_bytes());
            self.bytes.extend_from_slice(&row);
            self.readings += 1;
            self
        }

        fn finish(mut self) -> Vec<u8> {
            let header_size = size_of::<Header>() as u32;
            let sensor_size = size_of::<Sensor>() as u32;
            let reading_size = size_of::<layout::Reading>() as u32;
            let off_sensors = header_size;
            let off_readings = off_sensors + sensor_size * self.sensors;
            self.bytes[20..24].copy_from_slice(&off_sensors.to_le_bytes());
            self.bytes[24..28].copy_from_slice(&sensor_size.to_le_bytes());
            self.bytes[28..32].copy_from_slice(&self.sensors.to_le_bytes());
            self.bytes[32..36].copy_from_slice(&off_readings.to_le_bytes());
            self.bytes[36..40].copy_from_slice(&reading_size.to_le_bytes());
            self.bytes[40..44].copy_from_slice(&self.readings.to_le_bytes());
            self.bytes
        }
    }

    #[test]
    fn read_all_resolves_every_field_from_a_synthetic_block() {
        let bytes = Fixture::new()
            .sensor(10, 0, "Water Loop")
            .sensor(11, 1, "GPU [#1]")
            .reading(1, 0, 500, "Coolant", b"\xb0C", 32.5, 20.0, 45.0, 31.0)
            .reading(5, 1, 501, "GPU Power", b"W", 220.0, 0.0, 320.0, 180.0)
            .finish();

        let readings = parse::read_all(&bytes).unwrap();
        assert_eq!(readings.len(), 2);

        assert_eq!(readings[0].id, 500);
        assert_eq!(readings[0].sensor, "Water Loop");
        assert_eq!(readings[0].sensor_id, 10);
        assert_eq!(readings[0].sensor_instance, 0);
        assert_eq!(readings[0].label, "Coolant");
        assert_eq!(readings[0].unit, "°C");
        assert_eq!(readings[0].value, 32.5);
        assert_eq!(readings[0].value_min, 20.0);
        assert_eq!(readings[0].value_max, 45.0);
        assert_eq!(readings[0].value_avg, 31.0);
        assert_eq!(readings[0].kind, ReadingKind::Temperature);

        assert_eq!(readings[1].sensor, "GPU [#1]");
        assert_eq!(readings[1].sensor_id, 11);
        assert_eq!(readings[1].sensor_instance, 1);
        assert_eq!(readings[1].kind, ReadingKind::Power);
    }

    #[test]
    fn sensors_are_available_on_their_own_without_reading_the_table() {
        let bytes = Fixture::new().sensor(7, 2, "CPU").finish();
        let header = parse::header_checked(&bytes).unwrap();
        let sensors = parse::sensors(&bytes, header).unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].id, 7);
        assert_eq!(sensors[0].instance, 2);
        assert_eq!(sensors[0].name, "CPU");
    }

    #[test]
    fn a_reading_pointing_past_the_sensor_table_still_parses() {
        // HWiNFO's own index, not ours to validate — an out-of-range one
        // should read as an unnamed sensor rather than fail the whole table.
        let bytes = Fixture::new()
            .reading(1, 99, 1, "Orphan", b"C", 1.0, 0.0, 2.0, 1.0)
            .finish();
        let readings = parse::read_all(&bytes).unwrap();
        assert_eq!(readings[0].sensor, "");
        assert_eq!(readings[0].sensor_id, 0);
    }
}
