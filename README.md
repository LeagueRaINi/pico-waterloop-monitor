# Pico waterloop monitor

A Raspberry Pi Pico with a Waveshare Pico LCD 2 (320×240, ST7789) showing custom-loop
coolant temperature from an NTC thermistor, alongside CPU and GPU hotspot temperatures,
power and pump RPM pulled from HWiNFO on the PC.

![Rendered preview of the display: coolant temperature, CPU/GPU/pump readout, a
dual-scale temperature graph and a power graph](docs/preview.png)

The image above is rendered straight from the firmware by
[`tools/render_preview.py`](tools/render_preview.py) — same drawing code and font
rasteriser as the device, run against synthetic data on desktop Python with only the
SPI/GPIO calls stubbed out — so it can't drift from what's actually on the panel. It
lives outside `firmware/` because the Rust bridge's `include_dir!` copies that whole
directory to the device verbatim. Re-run it after any display change:

```bash
pip install pillow
python tools/render_preview.py
```

## Why

Most enthusiast boards expose a thermistor header. Mine does not — and coolant temperature
is the one number that tells you how a loop is actually doing, since CPU and GPU move with
load second to second while the water tells you whether the radiators are keeping up.

A Pico has an ADC, so it reads the thermistor itself. It has no network, but it is plugged
into the PC for power anyway and MicroPython presents that USB port as a serial console —
so a small Windows program pushes HWiNFO's readings down the same cable.

## What it shows

Two panels over a shared 30-minute clock, 10-second buckets, newest pinned to the right.

- **Temperatures** — coolant, CPU and GPU. Two scales: silicon down the left, coolant down
  the right in its own colour, so a couple of degrees of water is not squashed flat by tens
  of degrees of silicon. Where coolant crosses a silicon line it blends into it rather
  than covering it.
- **Power** — CPU package and GPU board power, directly below on the same time axis, so
  load and its effect on the water line up vertically.

CPU and GPU labels carry their trace colours, so neither graph needs a legend. Pump RPM
turns red below 300. With no PC link the coolant half still works and the PC rows grey out.

After five minutes without PC data the panel goes dark — the backlight is what ages here
and most of what the board draws. Sampling carries on while it is off, so waking shows a
real 30 minutes rather than an empty graph. PC data returning wakes it, as does any of the
four buttons. A Pico that has *never* heard from a PC is left alone: that is standalone
use, where blanking would just look broken.

The top-right button cycles the temperature and power graphs through four windows —
30 minutes, 15 minutes, 5 minutes, 30 seconds — each stretched across the same graph
width. The three longer ones are trailing slices of the same history already being
collected; the 30-second one draws from a small dedicated 1-second-resolution buffer,
since 10-second buckets would give it only 3 points. Which window is active shows in the
power graph's top-right corner. Bottom-right blanks the panel immediately instead of
waiting out the idle timeout. The other two just wake it, like any button did before.
While dark, all four are equivalent: any press wakes the panel, and only the press after
that does its own job.

## Hardware

| | |
| --- | --- |
| Display | Waveshare Pico LCD 2 — 320×240 ST7789, SPI1, `sck=10 mosi=11 cs=9 dc=8 rst=12 bl=13` |
| Thermistor | 10 kΩ NTC (B = 3435) on **GP26 / ADC0**, in a divider with a 10 kΩ fixed resistor to 3V3 |
| Pump tacho | Wired to the motherboard's CPU fan header, so HWiNFO can read it |

## Setup

**1. Flash MicroPython** — hold BOOTSEL, plug in, drop the `.uf2` from
[micropython.org](https://micropython.org/download/) onto the `RPI-RP2` drive.

**2. Get the bridge** — download the zip from the
[releases page](../../releases) and extract it anywhere, or build it:

```bash
cargo build --release --manifest-path pc/Cargo.toml
```

**3. Copy the firmware to the Pico.** `firmware/` is baked into both binaries, so this
needs nothing else present — no Python, no `mpremote`, no Thonny.

```powershell
hwinfo-pico-bridge.exe --deploy
```

**4. Enable HWiNFO shared memory** — Settings → Main Settings → **Shared Memory Support**.
That is a registered-version feature, and is how the bridge reads sensors. Leave HWiNFO
running; "Sensors-only" with auto-start is convenient.

**5. Run it at login.** This writes `HwinfoPicoBridge` to `HKCU\...\CurrentVersion\Run`, so
the tray app starts in your own session — a real service would sit in session 0 with no
desktop for the icon. Sensor options given alongside are recorded too.

```powershell
hwinfo-pico-bridge.exe --install
```

## The tray app

| Icon | |
| --- | --- |
| Teal | Data flowing to the Pico |
| Amber | HWiNFO is fine, the Pico link is not |
| Red | HWiNFO is unavailable |
| Grey | Paused, or busy updating the Pico |

Hover for live readings. Right-click for:

| | |
| --- | --- |
| **Pause** / **Resume** | Releases the serial port, so Thonny or a terminal can have the device without closing the app |
| **Update Pico** | Deploys this build's firmware, pausing and resuming around itself |
| **Exit** | Declines while an update is in flight |

> Windows 11 hides new tray icons in the overflow (`^`) area. To pin it: Settings →
> Personalization → Taskbar → Other system tray icons.

## Commands

The console binary does the lot and `-h` lists it all; the tray one takes the sensor
options only.

| | |
| --- | --- |
| `--list` | Every temperature, fan and power sensor HWiNFO exposes |
| `--cpu` `--gpu` `--pump` `--cpu-power` `--gpu-power` | Override a sensor by label |
| `--port COMn` `--interval <secs>` `--dry-run` `--force` | |
| `--install` `--uninstall` `--status` | Start-at-login registration |
| `-V` | Which build this is |
| `--deploy` `--deploy-list` `--deploy-reset` | Firmware on the device |
| `--firmware <dir>` | Deploy a working copy instead of the built-in one |
| `--all` `--verify` `--no-reset` | Deploy modifiers |

Sensors are picked automatically — hotspot readings, whole-package power, and GPU *board*
power rather than chip power, since the coolant sees everything the card dissipates. On
AMD, `CPU (Tctl/Tdie)` **is** the hotspot; `CPU IOD Hotspot` is deliberately not preferred.

Only one program can hold the serial port, so pause the tray app before deploying from the
console — or use its **Update Pico** item, which does that for you.

## Updating the firmware

A full copy is ~165 KB, nearly all font tables that change roughly never, and the REPL
takes a kilobyte per round trip. So a deploy records what it sent — sha256 and size per
file — in `/.pico-deploy` **on the device**, and the next one uploads only what differs.
Changing `main.py` uploads `main.py`. It lives on the Pico because the Pico is the thing
whose contents are in question, so any machine can update any device correctly.

The record is not trusted blindly: a file must also still be the size it claims, and
everything uploaded is hashed *by the Pico* and compared, so a truncated transfer fails
the deploy rather than leaving a broken display. `--verify` ignores the record and
rehashes everything; `--all` uploads regardless.

Reading any of that needs the REPL, and reaching the REPL stops `main.py`, which cannot
resume without a soft reset that empties the graphs. So an update first *asks* the running
display whether it already holds this exact set of files — if so, nothing is interrupted.
Firmware too old to answer, or a Pico already at a prompt, falls through to the REPL.

Files a previous deploy put there and that no longer exist in `firmware/` are removed;
anything else on the device is left alone.

> `--deploy` uses the copy compiled into the binary. When changing `firmware/`, rebuild
> first or pass `--firmware .\firmware`.

## Wire protocol

One ASCII line per update, to the Pico's USB serial console. `-` means unavailable, and
trailing fields may be missing entirely, so an older bridge still drives current firmware.
After 8 seconds of silence the display greys the PC rows and breaks the graph line rather
than inventing values.

```
T,<cpu>,<gpu>,<pump>,<cpuW>,<gpuW>\n     e.g.  T,64.5,71.0,1532,142,318
```

The updater asks one question over the same link, so that an update with nothing to send
does not have to stop the display to discover that:

```
?M   ->   M,<sha256 of the deploy record>      or   M,-
```

## Troubleshooting

| Symptom | Cause |
| --- | --- |
| `access denied` on the COM port | Something else holds it. Pause the tray app, or use **Update Pico** |
| `HWiNFO shared memory not found` | HWiNFO isn't running, or Shared Memory Support is off |
| Display shows `--` but the bridge says connected | The Pico is at the REPL, not running `main.py`. Use `--deploy-reset` |
| A deploy says "already up to date" but the display is wrong | The record disagrees. `--verify` rechecks it; `--all` skips it |
| No tray icon after login | Probably in the overflow area. Confirm with `--status` |
| `No Pico found` | Not enumerated as USB serial — check the cable carries data, and that MicroPython is flashed |

A port is only written to if enumeration reports it as USB vendor ID `2E8A`, so a wrong
`--port` cannot spray text at whatever else owns it. `--force` overrides.

## Tuning

Constants at the top of [`firmware/main.py`](firmware/main.py):

| | |
| --- | --- |
| `WINDOW_SECONDS` `HISTORY_POINTS` | Graph timeframe, and points across it |
| `SAMPLE_PERIOD` `GRAPH_PERIOD_MS` | ADC and redraw periods |
| `PC_TIMEOUT_MS` `PUMP_MIN_RPM` | Staleness cutoff, and the RPM below which the pump reads red |
| `STANDBY_AFTER_MS` | Unlinked time before the panel goes dark; `0` keeps it lit |
| `B` `R0` `R_FIXED` | Thermistor calibration |

## Notes

`firmware/` is copied to the device root verbatim, which is why the deploy script sits
outside it. `pc/` is a Cargo workspace: `hwinfo` reads HWiNFO's shared memory on its own,
with no dependency on anything Pico-specific, and is reusable wherever HWiNFO readings are
wanted by themselves; `hwinfo-pico-bridge-core` is the sensor picking, sampling loop,
serial transport and firmware updater the two front ends share; `hwinfo-pico-bridge` and
`hwinfo-pico-bridge-tray` are just their thin `main.rs`. The bridge leans on mainstream
crates (`serialport`, `tray-icon`, `winreg`, `sha2`, `base64`, `memchr`, `walkdir`,
`windows-sys`); the exception is [`hwinfo`](pc/hwinfo/src/lib.rs), which walks HWiNFO's
shared memory directly because HWiNFO exposes a raw layout rather than an API. That block
is *packed*, not MSVC-aligned as the published headers suggest, so it strides by the
element sizes the header reports and decodes only the fixed prefix. Verified against
shared memory version 2 revision 1.

Sensors resolve to table indices once and are rechecked each round: HWiNFO renumbers when
it rescans or a GPU wakes, and a stale index does not fail — it silently reports another
sensor's value. Pausing is a handshake for the same reason it exists: the tray waits for
the loop to confirm it has let go of the port before anything else opens it.

The firmware's drawing primitives are `@micropython.viper`, and allocate nothing per call.
Buffers are RGB565 big-endian while the RP2040 is little-endian, so a pixel is one 16-bit
store of a byte-swapped value.

Fonts under `firmware/lib/st7789/romfonts/` were trimmed to the three sizes `main.py`
imports; the full set is at
[russhughes/st7789py_mpy](https://github.com/russhughes/st7789py_mpy).

## Licence

MIT — see [LICENSE](LICENSE). Includes the MIT-licensed st7789py driver by Russ Hughes and
Ivan Belokobylskiy.
