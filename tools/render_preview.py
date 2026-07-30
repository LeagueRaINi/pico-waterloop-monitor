#!/usr/bin/env python3
"""
Render a PNG preview of the display straight from the firmware.

This does not reimplement any drawing logic. It shims the handful of things
that only exist on the Pico - `machine.Pin`/`SPI`/`ADC`, the `micropython`
module, and the ST7789 driver's SPI transport - then imports `main.py` and
`st7789py.py` completely unmodified and drives their real functions
(`draw_static_screen`, `draw_temperature`, `draw_pc_panel`, `draw_temp_graph`,
`draw_power_graph`, and the ST7789 driver underneath them) with synthetic
sensor data. The only "fake" part is where SPI bytes would leave the chip;
everything upstream of that, including the ST7789 driver's own font
rasteriser, is the genuine firmware.

Lives outside firmware/ deliberately: the Rust bridge's `include_dir!` embeds
that whole tree and copies it to the device verbatim, and this script - with
its desktop-only Pillow dependency - is not firmware.

Usage:
    pip install pillow
    python tools/render_preview.py [output.png]

Run this after any display-related change instead of hand-checking layout
on hardware, and after any change worth showing in the README.
"""
import builtins
import importlib
import inspect
import random
import struct
import sys

sys.dont_write_bytecode = True  # firmware/ deploys verbatim; no __pycache__ left behind
import time as _time
import types
from array import array
from functools import wraps
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
FIRMWARE_DIR = REPO_ROOT / "firmware"
OUT_PATH = Path(sys.argv[1]) if len(sys.argv) > 1 else REPO_ROOT / "docs" / "preview.png"

sys.path.insert(0, str(FIRMWARE_DIR / "lib"))
sys.path.insert(0, str(FIRMWARE_DIR))

# =========================================================
# ptr8/ptr16/ptr32 - MicroPython viper's pointer-cast pseudo-builtins.
#
# On the device these are recognised by the viper compiler; annotating a
# parameter with one gives the function a raw, unit-sized view over whatever
# buffer-like object was passed in. `memoryview(...).cast(fmt)` is the exact
# desktop equivalent for a byte buffer; array('H')/array('i') scratch
# buffers are already in the target format, so those pass through as-is.
# =========================================================
class _Ptr:
    def __init__(self, fmt):
        self.fmt = fmt

    def __call__(self, buf):
        mv = memoryview(buf)
        return mv if mv.format == self.fmt else mv.cast(self.fmt)


builtins.ptr8 = _Ptr("B")
builtins.ptr16 = _Ptr("H")
builtins.ptr32 = _Ptr("i")
builtins.uint = int


def _viper(func):
    """
    `@micropython.viper` for main.py: does the ptr8/16/32 param casting a
    real viper function gets from the compiler, based on the same
    annotations. Non-pointer viper functions (none in this firmware) would
    just pass through unchanged.
    """
    params = list(inspect.signature(func).parameters.items())
    casts = [
        (i, ann) for i, (_, p) in enumerate(params)
        if (ann := p.annotation) in (builtins.ptr8, builtins.ptr16, builtins.ptr32)
    ]
    if not casts:
        return func

    @wraps(func)
    def wrapper(*args):
        args = list(args)
        for i, ann in casts:
            args[i] = ann(args[i])
        return func(*args)

    return wrapper


micropython_stub = types.ModuleType("micropython")
micropython_stub.viper = _viper
micropython_stub.native = lambda func: func
micropython_stub.const = lambda x: x
sys.modules["micropython"] = micropython_stub

# =========================================================
# machine - Pin/SPI/ADC are only ever poked at, never read back except
# Pin.value() on the backlight, which nothing here inspects.
# =========================================================
machine_stub = types.ModuleType("machine")


class _Pin:
    IN, OUT, PULL_UP, PULL_DOWN = 0, 1, 2, 3

    def __init__(self, *a, **k):
        pass

    def on(self):
        pass

    def off(self):
        pass

    def value(self, v=None):
        return 0


class _SPI:
    def __init__(self, *a, **k):
        pass

    def write(self, data):
        pass


class _ADC:
    def __init__(self, *a, **k):
        pass

    def read_u16(self):
        return 32768


machine_stub.Pin = _Pin
machine_stub.SPI = _SPI
machine_stub.ADC = _ADC
sys.modules["machine"] = machine_stub

# =========================================================
# ST7789 SPI transport -> an in-memory framebuffer.
#
# Everything the driver draws (fill, fill_rect, blit_buffer, text, ...)
# funnels through _set_window (CASET/RASET/RAMWR) and _write(command, data).
# Replaying that same command stream into a 2D buffer is a hardware-protocol
# detail, not a rendering one - the actual pixels come entirely from the
# real driver and font code above it.
# =========================================================
_CASET, _RASET, _RAMWR = b"\x2a", b"\x2b", b"\x2c"


class FrameCapture:
    def __init__(self, w, h):
        self.w, self.h = w, h
        self.buf = bytearray(w * h * 2)  # RGB565, big-endian - the wire format
        self._x0 = self._y0 = self._x1 = self._y1 = 0
        self._cx = self._cy = 0

    def write(self, command, data):
        if command == _CASET:
            self._x0, self._x1 = struct.unpack(">HH", data)
        elif command == _RASET:
            self._y0, self._y1 = struct.unpack(">HH", data)
        elif command == _RAMWR:
            self._cx, self._cy = self._x0, self._y0
        elif command is None and data:
            off = 0
            n = len(data) // 2
            for _ in range(n):
                if self._cy > self._y1:
                    break
                if 0 <= self._cx < self.w and 0 <= self._cy < self.h:
                    idx = (self._cy * self.w + self._cx) * 2
                    self.buf[idx:idx + 2] = data[off:off + 2]
                off += 2
                self._cx += 1
                if self._cx > self._x1:
                    self._cx = self._x0
                    self._cy += 1


import st7789.st7789py as st7789py  # noqa: E402

# st7789py's own compat shim (for building docs without hardware) defines
# const/uint/micropython locally when `from time import sleep_ms` fails, as
# it just did on desktop CPython - so this patch must land after that
# import, not before, or it would mask the ImportError that triggers it.
if not hasattr(_time, "ticks_ms"):
    _time.ticks_ms = lambda: int(_time.monotonic() * 1000)
    _time.ticks_diff = lambda a, b: a - b
    _time.ticks_add = lambda a, b: a + b
    _time.sleep_ms = lambda ms: None

capture = FrameCapture(320, 240)


def _captured_write(self, command=None, data=None):
    capture.write(command, data)


st7789py.ST7789._write = _captured_write

import st7789.config.tft_buttons  # noqa: E402,F401  (import-safety check only)

main = importlib.import_module("main")  # runs module setup, main() is guarded

# =========================================================
# Synthetic 15-minute history, shaped like a loop under load: coolant
# creeping up as CPU/GPU ramp, then the radiators catching up.
# =========================================================
random.seed(2)


def wander(n, lo, hi, start=None, step=0.05):
    v = start if start is not None else (lo + hi) / 2
    out = []
    for _ in range(n):
        v += random.uniform(-1, 1) * (hi - lo) * step
        v = max(lo, min(hi, v))
        out.append(v)
    return out


n = main.HISTORY_POINTS
cpu_hist = wander(n, 38, 84, start=45)
gpu_hist = wander(n, 35, 78, start=42)
loop_hist = wander(n, 24.0, 30.5, start=25.0, step=0.02)
cpu_w_hist = wander(n, 25, 165, start=60)
gpu_w_hist = wander(n, 20, 240, start=80)

main.cpu_s.hist[:] = array("f", cpu_hist)
main.gpu_s.hist[:] = array("f", gpu_hist)
main.loop_s.hist[:] = array("f", loop_hist)
main.cpu_w_s.hist[:] = array("f", cpu_w_hist)
main.gpu_w_s.hist[:] = array("f", gpu_w_hist)

# The fine (30 s level) buffers aren't fed by the main loop here, so seed
# them too - otherwise the shortest zoom level would render as empty.
fn = main.FINE_POINTS
main.cpu_fine.hist[:] = array("f", wander(fn, 38, 84, start=cpu_hist[-1]))
main.gpu_fine.hist[:] = array("f", wander(fn, 35, 78, start=gpu_hist[-1]))
main.loop_fine.hist[:] = array("f", wander(fn, 24.0, 30.5, start=loop_hist[-1], step=0.02))
main.cpu_w_fine.hist[:] = array("f", wander(fn, 25, 165, start=cpu_w_hist[-1]))
main.gpu_w_fine.hist[:] = array("f", wander(fn, 20, 240, start=gpu_w_hist[-1]))

main.pc_cpu = cpu_hist[-1]
main.pc_gpu = gpu_hist[-1]
main.pc_pump = 1540
main.pc_cpu_w = cpu_w_hist[-1]
main.pc_gpu_w = gpu_w_hist[-1]
main.pc_last_ms = _time.ticks_ms()

# =========================================================
# Drive exactly what main()'s loop drives, in the same order.
# =========================================================
main.draw_static_screen()

shown = loop_hist[-1]
main.draw_temperature(shown)
main.tft.blit_buffer(main.temp_buf, main.TEMP_BUF_SCREEN_X, main.TEMP_BUF_SCREEN_Y,
                      main.TEMP_BUF_W, main.TEMP_BUF_H)

main.draw_pc_panel()
main.tft.blit_buffer(main.pc_buf, main.PC_X, main.PC_Y, main.PC_BUF_W, main.PC_BUF_H)

main.draw_temp_graph()
main.draw_power_graph()

# =========================================================
# RGB565 big-endian -> PNG.
# =========================================================
from PIL import Image  # noqa: E402

pixels = array("H")
pixels.frombytes(bytes(capture.buf))
if sys.byteorder == "little":
    pixels.byteswap()  # array('H') is native-endian; the buffer is big-endian

img = Image.new("RGB", (capture.w, capture.h))
dst = img.load()
for y in range(capture.h):
    row = y * capture.w
    for x in range(capture.w):
        v = pixels[row + x]
        r = (v >> 11) & 0x1F
        g = (v >> 5) & 0x3F
        b = v & 0x1F
        dst[x, y] = ((r * 255) // 31, (g * 255) // 63, (b * 255) // 31)

OUT_PATH.parent.mkdir(parents=True, exist_ok=True)
img.save(OUT_PATH)
print(f"wrote {OUT_PATH}")
