#!/usr/bin/env python3
"""
Exercises the actual button state machine (`main._poll_buttons`) across
simulated ticks, with fake GPIO pins - not the visual output of a single
call like render_preview.py, but the tick-by-tick edge detection itself.

Uses the same hardware shims as render_preview.py (see there for why); this
file only adds scriptable fake buttons on top and drives `_poll_buttons()`
directly, since `main()` itself is an unbreakable `while True`.

Usage:
    python tools/test_buttons.py
"""
import builtins
import importlib
import inspect
import sys
import time as _time
import types
from functools import wraps
from pathlib import Path

sys.dont_write_bytecode = True  # firmware/ deploys verbatim; no __pycache__ left behind

REPO_ROOT = Path(__file__).resolve().parents[1]
FIRMWARE_DIR = REPO_ROOT / "firmware"

sys.path.insert(0, str(FIRMWARE_DIR / "lib"))
sys.path.insert(0, str(FIRMWARE_DIR))


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

import st7789.st7789py as st7789py  # noqa: E402

if not hasattr(_time, "ticks_ms"):
    _time.ticks_ms = lambda: int(_time.monotonic() * 1000)
    _time.ticks_diff = lambda a, b: a - b
    _time.ticks_add = lambda a, b: a + b
    _time.sleep_ms = lambda ms: None


def _noop_write(self, command=None, data=None):
    pass


st7789py.ST7789._write = _noop_write

import st7789.config.tft_buttons  # noqa: E402,F401  (import-safety check only)

main = importlib.import_module("main")  # runs module setup, main() is guarded


# =========================================================
# Scriptable fake button: active-low, like the real pull-ups.
# =========================================================
class FakeButton:
    def __init__(self):
        self.down = False

    def value(self):
        return 0 if self.down else 1


class Failure(AssertionError):
    pass


def check(condition, message):
    if not condition:
        raise Failure(message)


def tick():
    main._poll_buttons()


def reset(pc_linked=False):
    """A clean slate before each scenario."""
    main._standby = False
    main._zoomed = False
    main._prev_wake_pressed = False
    main._prev_zoom_pressed = False
    main._prev_sleep_pressed = False
    main._idle_ms = 0
    # None keeps _standby_due() permanently False (standalone-use rule),
    # so idle timeout never fires mid-scenario and confuses a result.
    main.pc_last_ms = _time.ticks_ms() if pc_linked else None
    for b in (zoom_btn, sleep_btn, wake_btn_a, wake_btn_b):
        b.down = False
    tick()  # let the all-released state settle into _prev_*


zoom_btn, sleep_btn, wake_btn_a, wake_btn_b = FakeButton(), FakeButton(), FakeButton(), FakeButton()
main._wake_keys = (zoom_btn, sleep_btn, wake_btn_a, wake_btn_b)
main._zoom_key = zoom_btn
main._sleep_key = sleep_btn

results = []


def scenario(name):
    def wrap(fn):
        reset()
        try:
            fn()
        except Failure as e:
            results.append((name, False, str(e)))
        else:
            results.append((name, True, ""))
        return fn
    return wrap


@scenario("zoom button toggles on press, not while held")
def _():
    zoom_btn.down = True
    tick()
    check(main._zoomed is True, f"expected zoomed after first press, got {main._zoomed}")
    tick()
    tick()
    check(main._zoomed is True, "holding the button re-toggled zoom")
    zoom_btn.down = False
    tick()
    check(main._zoomed is True, "releasing the button changed zoom")
    zoom_btn.down = True
    tick()
    check(main._zoomed is False, "second press did not toggle back")


@scenario("sleep-now enters standby on press")
def _():
    check(main._standby is False, "should start awake")
    sleep_btn.down = True
    tick()
    check(main._standby is True, "sleep-now press did not enter standby")


@scenario("holding sleep-now does not immediately wake it back up")
def _():
    # This is the bug: the sleep button is also a wake button (all four
    # are), so a naive level check sees it still held on the very next
    # tick and treats that as "a button woke it" - flickering the panel
    # off and straight back on for any press longer than one ~250ms tick.
    sleep_btn.down = True
    tick()
    check(main._standby is True, "did not enter standby on press")
    for _ in range(5):
        tick()
        check(main._standby is True,
              "standby cancelled itself while the same button was still held")
    sleep_btn.down = False
    tick()
    check(main._standby is True, "releasing the button alone should not wake it")


@scenario("a fresh press after release does wake it")
def _():
    sleep_btn.down = True
    tick()
    check(main._standby is True, "did not enter standby")
    sleep_btn.down = False
    tick()
    wake_btn_a.down = True
    tick()
    check(main._standby is False, "a new press on a different button did not wake it")


@scenario("waking via the zoom button does not also toggle zoom that tick")
def _():
    sleep_btn.down = True
    tick()
    check(main._standby is True, "setup: did not enter standby")
    sleep_btn.down = False
    tick()

    zoom_btn.down = True
    tick()
    check(main._standby is False, "zoom button held did not wake the panel")
    check(main._zoomed is False,
          "the same press that woke the panel also toggled zoom")

    tick()
    tick()
    check(main._zoomed is False,
          "continuing to hold the wake button toggled zoom on a later tick")

    zoom_btn.down = False
    tick()
    zoom_btn.down = True
    tick()
    check(main._zoomed is True, "a real press-after-release did not toggle zoom")


@scenario("pc link waking works independent of button edges")
def _():
    reset(pc_linked=False)
    sleep_btn.down = True
    tick()
    check(main._standby is True, "setup: did not enter standby")
    sleep_btn.down = False
    tick()
    main.pc_last_ms = _time.ticks_ms()  # the PC starts talking again
    tick()
    check(main._standby is False, "a fresh PC link did not wake the panel")

print(f"{len(results)} scenario(s):")
failed = 0
for name, ok, message in results:
    if ok:
        print(f"  ok    {name}")
    else:
        failed += 1
        print(f"  FAIL  {name}")
        print(f"        {message}")

if failed:
    print(f"\n{failed} of {len(results)} failed")
    sys.exit(1)
print("\nall passed")
