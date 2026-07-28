//! Lists every reading HWiNFO is currently publishing, grouped by kind.
//!
//! A minimal example of using this crate on its own, outside the pico
//! bridge — and a quick way to eyeball what a real HWiNFO instance reports
//! for the reading types the bridge itself never needed (voltage, current,
//! clock, usage), to check `ReadingKind::from_raw` against real data rather
//! than the reverse-engineered header alone.
//!
//!     cargo run -p hwinfo --example dump

use hwinfo::{ReadingKind, SharedMem};
use std::collections::BTreeMap;

fn main() {
    let shm = match SharedMem::open() {
        Ok(shm) => shm,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };

    let readings = shm.read_all().unwrap_or_else(|err| {
        eprintln!("{err}");
        std::process::exit(1);
    });

    let mut by_kind: BTreeMap<String, Vec<&hwinfo::Reading>> = BTreeMap::new();
    for r in &readings {
        by_kind.entry(r.kind.to_string()).or_default().push(r);
    }

    for (kind, group) in &by_kind {
        println!("== {kind} ({}) ==", group.len());
        for r in group.iter().take(3) {
            println!(
                "  {} / {} = {} {} (min {} max {} avg {})",
                r.sensor, r.label, r.value, r.unit, r.value_min, r.value_max, r.value_avg
            );
        }
        if group.len() > 3 {
            println!("  ... and {} more", group.len() - 3);
        }
    }

    // ReadingKind::Other should only ever be HWiNFO's own "other" type (raw
    // code 8) on a real instance — anything else here would mean the SDK has
    // grown a type this crate does not know about yet.
    let unmapped: Vec<_> = readings
        .iter()
        .filter_map(|r| match r.kind {
            ReadingKind::Other(raw) if raw != 8 => Some(raw),
            _ => None,
        })
        .collect();
    if !unmapped.is_empty() {
        println!("\nunrecognised raw type codes: {unmapped:?}");
    }
}
