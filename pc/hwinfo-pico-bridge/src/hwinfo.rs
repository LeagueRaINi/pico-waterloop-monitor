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

const SHM_NAME: &str = "Global\\HWiNFO_SENS_SM2";
const SIGNATURE: &[u8; 4] = b"HWiS";

// Header field offsets (packed, 44 bytes total).
const H_POLL_TIME: usize = 12;
const H_OFF_SENSORS: usize = 20;
const H_SIZE_SENSOR: usize = 24;
const H_NUM_SENSORS: usize = 28;
const H_OFF_READINGS: usize = 32;
const H_SIZE_READING: usize = 36;
const H_NUM_READINGS: usize = 40;
const HEADER_SIZE: usize = 44;

// Sensor element: fixed prefix is 264 bytes.
const S_NAME_ORIG: usize = 8;
const S_NAME_USER: usize = 136;
const SENSOR_FIXED: usize = 264;

// Reading element: fixed prefix is 316 bytes.
const R_TYPE: usize = 0;
const R_SENSOR_INDEX: usize = 4;
const R_LABEL_ORIG: usize = 12;
const R_LABEL_USER: usize = 140;
const R_UNIT: usize = 268;
const R_VALUE: usize = 284;
const READING_FIXED: usize = 316;

const STRING_LEN: usize = 128;
const UNIT_LEN: usize = 16;

pub const READING_TYPE_TEMPERATURE: u32 = 1;
pub const READING_TYPE_FAN: u32 = 3;
pub const READING_TYPE_POWER: u32 = 5;

pub struct Reading {
    pub sensor: String,
    pub label: String,
    pub unit: String,
    pub value: f64,
    pub kind: u32,
}

impl Reading {
    pub fn is_temperature(&self) -> bool {
        self.kind == READING_TYPE_TEMPERATURE
    }

    pub fn is_fan(&self) -> bool {
        self.kind == READING_TYPE_FAN
    }

    pub fn is_power(&self) -> bool {
        self.kind == READING_TYPE_POWER
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

    fn bytes_at(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        if end > self.len {
            return None;
        }
        // SAFETY: the range is inside the mapped, committed view.
        Some(unsafe { std::slice::from_raw_parts(self.base.add(offset), len) })
    }

    fn u32_at(&self, offset: usize) -> Option<u32> {
        let b = self.bytes_at(offset, 4)?;
        Some(u32::from_le_bytes(b.try_into().ok()?))
    }

    fn i64_at(&self, offset: usize) -> Option<i64> {
        let b = self.bytes_at(offset, 8)?;
        Some(i64::from_le_bytes(b.try_into().ok()?))
    }

    fn f64_at(&self, offset: usize) -> Option<f64> {
        let b = self.bytes_at(offset, 8)?;
        Some(f64::from_le_bytes(b.try_into().ok()?))
    }

    /// HWiNFO writes these as single-byte characters (0xB0 for the degree
    /// sign), i.e. Latin-1 rather than UTF-8.
    fn string_at(&self, offset: usize, max_len: usize) -> String {
        let Some(raw) = self.bytes_at(offset, max_len) else {
            return String::new();
        };
        let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        raw[..end]
            .iter()
            .map(|&b| b as char)
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// True while the block still belongs to a live HWiNFO instance. It flips
    /// to "DEAD" on shutdown, and a restarted HWiNFO creates a *new* section,
    /// leaving ours frozen — which the caller detects via `poll_time`.
    pub fn is_valid(&self) -> bool {
        self.bytes_at(0, 4) == Some(SIGNATURE.as_slice())
    }

    pub fn poll_time(&self) -> i64 {
        self.i64_at(H_POLL_TIME).unwrap_or(0)
    }

    pub fn read_all(&self) -> Result<Vec<Reading>, String> {
        if !self.is_valid() {
            return Err("HWiNFO shared memory went away".into());
        }

        let get = |off: usize| self.u32_at(off).unwrap_or(0) as usize;
        let off_sensors = get(H_OFF_SENSORS);
        let size_sensor = get(H_SIZE_SENSOR);
        let num_sensors = get(H_NUM_SENSORS);
        let off_readings = get(H_OFF_READINGS);
        let size_reading = get(H_SIZE_READING);
        let num_readings = get(H_NUM_READINGS);

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
        let num_sensors = num_sensors.min(fits(off_sensors, size_sensor));
        let num_readings = num_readings.min(fits(off_readings, size_reading));

        let mut sensors = Vec::with_capacity(num_sensors);
        for i in 0..num_sensors {
            let base = off_sensors + i * size_sensor;
            let user = self.string_at(base + S_NAME_USER, STRING_LEN);
            let name = if user.is_empty() {
                self.string_at(base + S_NAME_ORIG, STRING_LEN)
            } else {
                user
            };
            sensors.push(name);
        }

        let mut readings = Vec::with_capacity(num_readings);
        for i in 0..num_readings {
            let base = off_readings + i * size_reading;
            let Some(kind) = self.u32_at(base + R_TYPE) else {
                break;
            };
            let sensor_index = self.u32_at(base + R_SENSOR_INDEX).unwrap_or(0) as usize;
            let user = self.string_at(base + R_LABEL_USER, STRING_LEN);
            let label = if user.is_empty() {
                self.string_at(base + R_LABEL_ORIG, STRING_LEN)
            } else {
                user
            };
            let Some(value) = self.f64_at(base + R_VALUE) else {
                break;
            };
            readings.push(Reading {
                sensor: sensors.get(sensor_index).cloned().unwrap_or_default(),
                label,
                unit: self.string_at(base + R_UNIT, UNIT_LEN),
                value,
                kind,
            });
        }
        Ok(readings)
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
