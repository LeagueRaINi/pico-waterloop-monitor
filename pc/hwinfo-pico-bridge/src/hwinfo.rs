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

use std::ffi::c_void;
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Memory::{
    MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
    MEMORY_BASIC_INFORMATION, MEM_COMMIT,
};
use zerocopy::{FromBytes, Immutable, KnownLayout};

const SHM_NAME: &str = "Global\\HWiNFO_SENS_SM2";
const SIGNATURE: [u8; 4] = *b"HWiS";

const STRING_LEN: usize = 128;
const UNIT_LEN: usize = 16;

/// What a reading measures.
///
/// HWiNFO publishes many more types than these — voltages, clocks, usage,
/// currents — and the display has no use for any of them, so they arrive as
/// `Other` rather than being enumerated.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReadingKind {
    Temperature,
    Fan,
    Power,
    Other(u32),
}

impl ReadingKind {
    /// The type codes as HWiNFO writes them into the `SENSOR_READING_TYPE`
    /// field. A data-carrying variant rules out `#[repr(u32)]` discriminants,
    /// so the mapping is spelled out here.
    fn from_raw(raw: u32) -> ReadingKind {
        match raw {
            1 => ReadingKind::Temperature,
            3 => ReadingKind::Fan,
            5 => ReadingKind::Power,
            other => ReadingKind::Other(other),
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

pub struct Reading {
    pub sensor: String,
    pub label: String,
    pub unit: String,
    pub value: f64,
    pub kind: ReadingKind,
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

    fn bytes_at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }
        // SAFETY: the range is inside the mapped, committed view.
        Some(unsafe { std::slice::from_raw_parts(self.base.add(offset), len) })
    }

    /// One of the `layout` structures, read in place. `None` means the element
    /// would run past the end of the mapped view.
    fn struct_at<T: FromBytes + KnownLayout + Immutable>(&self, offset: usize) -> Option<&T> {
        let bytes = self.bytes_at(offset, std::mem::size_of::<T>())?;
        T::ref_from_prefix(bytes).ok().map(|(value, _rest)| value)
    }

    fn header(&self) -> Option<&Header> {
        self.struct_at::<Header>(0)
    }

    /// True while the block still belongs to a live HWiNFO instance. It flips
    /// to "DEAD" on shutdown, and a restarted HWiNFO creates a *new* section,
    /// leaving ours frozen — which the caller detects via `poll_time`.
    pub fn is_valid(&self) -> bool {
        self.header()
            .is_some_and(|header| header.signature == SIGNATURE)
    }

    pub fn poll_time(&self) -> i64 {
        self.header().map_or(0, |header| header.poll_time.get())
    }

    pub fn read_all(&self) -> Result<Vec<Reading>, String> {
        let Some(header) = self.header().filter(|h| h.signature == SIGNATURE) else {
            return Err("HWiNFO shared memory went away".into());
        };

        let off_sensors = header.offset_sensors.get() as usize;
        let size_sensor = header.size_sensor.get() as usize;
        let off_readings = header.offset_readings.get() as usize;
        let size_reading = header.size_reading.get() as usize;

        if size_sensor < SENSOR_FIXED || size_reading < READING_FIXED {
            return Err(format!(
                "unexpected HWiNFO layout (sensor {size_sensor} B, reading \
                 {size_reading} B) — this build of HWiNFO is not supported"
            ));
        }

        // The counts are 32-bit fields read out of a block another process is
        // writing, so they can be caught mid-update or simply be nonsense.
        // Believing one would mean reserving up to 4 G elements: an allocation
        // failure, which aborts the process outright under `panic = "abort"`.
        // Cap them at what the mapped view could physically hold — the element
        // walk below is bounds checked anyway, so this only has to be sane.
        let fits = |offset: usize, stride: usize| self.len.saturating_sub(offset) / stride;
        let num_sensors = (header.num_sensors.get() as usize).min(fits(off_sensors, size_sensor));
        let num_readings =
            (header.num_readings.get() as usize).min(fits(off_readings, size_reading));

        let mut sensors = Vec::with_capacity(num_sensors);
        for i in 0..num_sensors {
            let Some(sensor) = self.struct_at::<Sensor>(off_sensors + i * size_sensor) else {
                break;
            };
            sensors.push(preferred(&sensor.name_user, &sensor.name_orig));
        }

        let mut readings = Vec::with_capacity(num_readings);
        for i in 0..num_readings {
            let Some(raw) = self.struct_at::<layout::Reading>(off_readings + i * size_reading)
            else {
                break;
            };
            readings.push(Reading {
                sensor: sensors
                    .get(raw.sensor_index.get() as usize)
                    .cloned()
                    .unwrap_or_default(),
                label: preferred(&raw.label_user, &raw.label_orig),
                unit: latin1(&raw.unit),
                value: raw.value.get(),
                kind: ReadingKind::from_raw(raw.kind.get()),
            });
        }
        Ok(readings)
    }
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
        assert_eq!(offset_of!(Sensor, name_orig), 8);
        assert_eq!(offset_of!(Sensor, name_user), 136);
    }

    #[test]
    fn a_reading_element_is_laid_out_where_hwinfo_puts_it() {
        assert_eq!(size_of::<layout::Reading>(), 316, "fixed prefix");
        assert_eq!(offset_of!(layout::Reading, kind), 0);
        assert_eq!(offset_of!(layout::Reading, sensor_index), 4);
        assert_eq!(offset_of!(layout::Reading, label_orig), 12);
        assert_eq!(offset_of!(layout::Reading, label_user), 140);
        assert_eq!(offset_of!(layout::Reading, unit), 268);
        assert_eq!(offset_of!(layout::Reading, value), 284);
    }

    #[test]
    fn the_reading_type_codes_are_the_ones_hwinfo_publishes() {
        // These used to be named constants; the match is now the only place
        // the numbers live, so this is what pins them.
        assert_eq!(ReadingKind::from_raw(1), ReadingKind::Temperature);
        assert_eq!(ReadingKind::from_raw(3), ReadingKind::Fan);
        assert_eq!(ReadingKind::from_raw(5), ReadingKind::Power);
        // Voltage, current, clock, usage and the rest: carried through rather
        // than enumerated, so an unknown type is never mistaken for a known one.
        assert_eq!(ReadingKind::from_raw(2), ReadingKind::Other(2));
        assert_eq!(ReadingKind::from_raw(0), ReadingKind::Other(0));
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
}
