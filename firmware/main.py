import math
import micropython
import sys
import time
from array import array
from machine import ADC

import st7789.st7789py as st7789
import st7789.config.tft_config as tft_config

import st7789.romfonts.vga1_8x8 as font_small
import st7789.romfonts.vga2_16x16 as font_med
import st7789.romfonts.vga2_bold_16x32 as font_big

# =========================================================
# THERMISTOR CONFIG
# =========================================================
ADC_PIN = 26
R_FIXED = 10_000
VCC     = 3.3
R0      = 10_000
T0      = 298.15
B       = 3435

ADC_OVERSAMPLE = 16     # ADC reads averaged per sample
EMA_ALPHA      = 0.25   # smoothing of the displayed coolant number

adc = ADC(ADC_PIN)

# =========================================================
# DISPLAY
# =========================================================
tft = tft_config.config(3)
W   = tft.width          # 320
H   = tft.height         # 240

# =========================================================
# COLORS
# =========================================================
BG         = st7789.color565(8, 10, 14)
PANEL      = st7789.color565(18, 22, 30)
GRID       = st7789.color565(28, 34, 42)
TEXT_DIM   = st7789.color565(130, 140, 150)
TEXT_BRIGHT= st7789.color565(215, 225, 235)
PUMP_COL   = st7789.color565(90, 200, 255)
COLD       = st7789.color565(80, 180, 255)
NORMAL     = st7789.color565(0, 220, 180)
HOT        = st7789.color565(255, 140, 60)
CRIT       = st7789.color565(255, 70, 70)
CPU_COL    = st7789.color565(255, 120, 90)
GPU_COL    = st7789.color565(150, 130, 255)
# Muted variants for the temperature panel, where coolant is the subject and
# the silicon is context. On the power panel they are the subject, so they
# keep their full-strength colours there.
CPU_MUTED  = st7789.color565(150, 70, 52)
GPU_MUTED  = st7789.color565(88, 76, 150)
STALE      = st7789.color565(90, 96, 105)

# =========================================================
# TIMEBASE  —  30 minute rolling window
#
# Sampling stays fast so the big number reacts, but samples are averaged into
# BUCKET_SECONDS-wide buckets; one bucket is one plotted point.
# =========================================================
WINDOW_SECONDS  = 30 * 60                            # 1800 s of history
HISTORY_POINTS  = 180                                # points across the graph
BUCKET_SECONDS  = WINDOW_SECONDS / HISTORY_POINTS    # 10 s per point
BUCKET_MS       = int(BUCKET_SECONDS * 1000)

# A dedicated 1-second-bucket buffer backs the shortest zoom level - the
# 10-second coarse buckets above would give it only 3 points across 30
# seconds, not enough to look like anything but straight lines. The other
# three levels are trailing slices of the coarse buffer above; no reason to
# pay for finer resolution than 5 or 15 or 30 minutes actually needs.
FINE_BUCKET_SECONDS = 1
FINE_BUCKET_MS      = int(FINE_BUCKET_SECONDS * 1000)
FINE_WINDOW_SECONDS = 60                                  # headroom over the 30 s level
FINE_POINTS         = int(FINE_WINDOW_SECONDS / FINE_BUCKET_SECONDS)   # 60 points

# (duration, bucket width, label) for what the zoom button cycles through.
ZOOM_LEVELS = (
    (WINDOW_SECONDS, BUCKET_SECONDS, "30m"),
    (15 * 60,        BUCKET_SECONDS, "15m"),
    (5 * 60,         BUCKET_SECONDS, "5m"),
    (30,             FINE_BUCKET_SECONDS, "30s"),
)
ZOOM_POINTS = tuple(max(2, int(d / b)) for d, b, _ in ZOOM_LEVELS)
ZOOM_LABELS = tuple(label for _, _, label in ZOOM_LEVELS)
ZOOM_FINE   = tuple(b == FINE_BUCKET_SECONDS for _, b, _ in ZOOM_LEVELS)

SAMPLE_PERIOD   = 0.25          # ADC / big number update period
GRAPH_PERIOD_MS = 2000          # graph panel redraw period
PC_TIMEOUT_MS   = 8000          # no serial data for this long -> "NO DATA"
STANDBY_AFTER_MS = 5 * 60 * 1000    # dark after this long unlinked; 0 disables

# =========================================================
# LAYOUT (320 x 240)
# =========================================================
TEMP_X, TEMP_Y     = 8, 8
TEMP_BUF_W         = 88         # big number + degree marker, no trend arrow
TEMP_BUF_H         = 40
TEMP_INSET_Y       = 4
TEMP_BUF_SCREEN_X  = TEMP_X
TEMP_BUF_SCREEN_Y  = TEMP_Y - TEMP_INSET_Y

LABEL_X, LABEL_Y   = 10, 48     # static "COOLANT" caption

# CPU / GPU / PUMP readouts in three columns: label, temperature, watts.
# There is no separate link indicator - when PC data goes stale every row
# greys out, which says the same thing.
#
# Values are right-aligned in fixed-width fields so the digits line up down
# the column, which is what sets the widths below: four characters for the
# middle column to fit a pump RPM, three for watts. Temperatures are whole
# degrees here; only the big coolant number keeps its decimal.
#
# The inter-column gaps are trimmed to the minimum that still keeps each
# group legible (down to 1px around the single-character units, which carry
# a colour change of their own to separate them) so the row's right edge
# lands in the same place the graphs below do, instead of running to the
# physical screen edge.
PC_X, PC_Y         = 112, 6
PC_BUF_W           = 200
PC_BUF_H           = 58
PC_COL_VALUE       = 66         # temperature / pump, clear of the 4-char "PUMP"
PC_COL_VALUE_UNIT  = 131
PC_COL_WATTS       = 143
PC_COL_WATTS_UNIT  = 192
PC_UNIT_DY         = 4          # centres the 8px unit against the 16px value
PC_ROW_Y           = (0, 21, 42)
PUMP_MIN_RPM       = 300        # below this the pump is considered stalled

GRAPH_X            = 8          # matches the coolant number's left edge
GRAPH_W            = 304        # both margins are 8px, same as the PC panel's right edge
# One graph for every temperature, one for power.
G1_Y,  G1_H        = 72,  76    # coolant + cpu + gpu, dual scale
G2_Y,  G2_H        = 160, 74    # cpu + gpu power

# =========================================================
# HISTORY
# One entry per bucket, oldest first, newest last.  An entry may be None,
# meaning "no data for that bucket" - the graph breaks the line there instead
# of inventing a value.  Every series shares the same bucket clock, so a given
# index is the same moment in time on both graphs.
# =========================================================
class Series:
    """
    A rolling window of bucket averages, plus the bucket being filled.

    `hist` is a packed `array('f')`, not a list - each entry costs 4 bytes
    instead of a boxed float object's ~20, which is what makes the second,
    finer-grained history buffer for the zoomed-in view affordable. Arrays
    cannot hold `None`, so a missing bucket is NaN instead; every reader
    of `hist` treats `v != v` (true only for NaN) as "no data here". This
    MicroPython build's array has no `.pop()` at all (confirmed on the
    actual device, not just from the docs) - trimming is a slice
    assignment to an empty array instead, which it does support.
    """

    __slots__ = ("hist", "_total", "_count", "_max")

    def __init__(self, max_points=HISTORY_POINTS):
        self.hist = array("f")
        self._total = 0.0
        self._count = 0
        self._max = max_points

    def add(self, value):
        if value is not None:
            self._total += value
            self._count += 1

    def _bucket_avg(self):
        return (self._total / self._count) if self._count else float("nan")

    def close(self):
        """Commit the open bucket to history and start a fresh one."""
        self.hist.append(self._bucket_avg())
        excess = len(self.hist) - self._max
        if excess > 0:
            self.hist[0:excess] = array("f")
        self._total = 0.0
        self._count = 0

    def push_live(self):
        """
        Temporarily append the still-open bucket so the graphs move between
        bucket boundaries instead of freezing for a whole bucket at a time.

        `hist` is allowed to run one over the cap - trimming it would
        discard a real bucket that pop_live() could not put back.  The
        plotter just lets the oldest point fall off the left edge.
        """
        self.hist.append(self._bucket_avg())

    def pop_live(self):
        if self.hist:
            self.hist[-1:] = array("f")


loop_s  = Series()      # coolant, from the thermistor
cpu_s   = Series()      # cpu hotspot
gpu_s   = Series()      # gpu hotspot
cpu_w_s = Series()      # cpu package power
gpu_w_s = Series()      # gpu board power

ALL_SERIES = (loop_s, cpu_s, gpu_s, cpu_w_s, gpu_w_s)

# Same five, at FINE_BUCKET_SECONDS resolution, for the shortest zoom level.
loop_fine  = Series(FINE_POINTS)
cpu_fine   = Series(FINE_POINTS)
gpu_fine   = Series(FINE_POINTS)
cpu_w_fine = Series(FINE_POINTS)
gpu_w_fine = Series(FINE_POINTS)

ALL_FINE_SERIES = (loop_fine, cpu_fine, gpu_fine, cpu_w_fine, gpu_w_fine)

# latest values from the PC bridge
pc_cpu     = None
pc_gpu     = None
pc_pump    = None      # pump RPM, off the CPU fan header
pc_cpu_w   = None
pc_gpu_w   = None
pc_last_ms = None

# =========================================================
# OFF-SCREEN BUFFERS (RGB565, big-endian)
# The two graph panels share one buffer: same width, so the shorter panel is
# exactly the first G2_H rows of it.  Each is drawn and blitted in turn.
# =========================================================
temp_buf  = bytearray(TEMP_BUF_W * TEMP_BUF_H * 2)
pc_buf    = bytearray(PC_BUF_W * PC_BUF_H * 2)
graph_buf = bytearray(GRAPH_W * max(G1_H, G2_H) * 2)

_G1_BYTES = GRAPH_W * G1_H * 2
_G2_BYTES = GRAPH_W * G2_H * 2

# =========================================================
# LOW-LEVEL BUFFER PRIMITIVES
#
# Buffers are RGB565 big-endian, which is what the panel wants off the wire.
# The RP2040 is little-endian, so one 16-bit store of a byte-swapped value
# does what two byte stores did - hence the ((lo << 8) | hi) shuffling below.
#
# They are viper, and allocate nothing per call: a redraw touches tens of
# thousands of pixels, and both the bytecode and the garbage it would make
# dominate everything else the firmware does. Viper takes at most four
# arguments, so anything wanting more is handed a preallocated array.
# =========================================================
@micropython.viper
def _fill16(buf: ptr16, start: int, count: int, color: int):
    """`count` pixels from pixel index `start`."""
    value = ((color & 0xFF) << 8) | ((color >> 8) & 0xFF)
    i = start
    end = start + count
    while i < end:
        buf[i] = value
        i = i + 1


@micropython.viper
def _stride16(buf: ptr16, start: int, count: int, packed: int):
    """`count` pixels from `start`, one every `packed >> 16` pixels."""
    stride = packed >> 16
    color = packed & 0xFFFF
    value = ((color & 0xFF) << 8) | ((color >> 8) & 0xFF)
    i = start
    n = count
    while n > 0:
        buf[i] = value
        i = i + stride
        n = n - 1


def _buf_fill(buf, color, nbytes=None):
    if nbytes is None:
        nbytes = len(buf)
    _fill16(buf, 0, nbytes >> 1, color)


def _buf_hline(buf, x, y, length, color, buf_w, buf_h):
    if not (0 <= y < buf_h):
        return
    x0 = x if x > 0 else 0
    x1 = x + length
    if x1 > buf_w:
        x1 = buf_w
    if x0 >= x1:
        return
    _fill16(buf, y * buf_w + x0, x1 - x0, color)


def _buf_vrun(buf, x, y0, y1, color, buf_w, buf_h):
    """Fill the vertical span y0..y1 (inclusive) of column x."""
    if not (0 <= x < buf_w):
        return
    if y0 < 0:
        y0 = 0
    if y1 > buf_h - 1:
        y1 = buf_h - 1
    if y0 > y1:
        return
    _stride16(buf, y0 * buf_w + x, y1 - y0 + 1, (buf_w << 16) | color)


# bw, bh, x, y, fw, fh, bytes_per_row, glyph offset, fg, bg (-1 transparent)
_GLYPH_META = array('i', bytearray(10 * 4))


@micropython.viper
def _glyph16(buf: ptr16, font_data: ptr8, meta: ptr32):
    buf_w = int(meta[0])
    buf_h = int(meta[1])
    x = int(meta[2])
    y = int(meta[3])
    fw = int(meta[4])
    fh = int(meta[5])
    bytes_per_row = int(meta[6])
    offset = int(meta[7])
    fg = int(meta[8])
    bg = int(meta[9])

    fg_value = ((fg & 0xFF) << 8) | ((fg >> 8) & 0xFF)
    bg_value = ((bg & 0xFF) << 8) | ((bg >> 8) & 0xFF)

    row = 0
    while row < fh:
        py = y + row
        if py >= 0:
            if py < buf_h:
                base = py * buf_w
                row_offset = offset + row * bytes_per_row
                col = 0
                while col < fw:
                    px = x + col
                    if px >= 0:
                        if px < buf_w:
                            bits = int(font_data[row_offset + (col >> 3)])
                            if (bits >> (7 - (col & 7))) & 1:
                                buf[base + px] = fg_value
                            elif bg >= 0:
                                buf[base + px] = bg_value
                    col = col + 1
        row = row + 1


def _buf_text(buf, font, string, x, y, fg, bg, buf_w, buf_h):
    """
    Render a fixed-width font into the buffer.
    Font format: WIDTH, HEIGHT, FIRST, LAST, FONT = memoryview of all glyph bitmaps.
    Bitmap: each glyph is stored row-major, each byte = 8 horizontal pixels (MSB left).
    """
    fw = font.WIDTH
    fh = font.HEIGHT
    first = font.FIRST
    last = font.LAST
    bytes_per_row = (fw + 7) // 8
    bytes_per_glyph = bytes_per_row * fh
    font_data = font.FONT          # memoryview

    meta = _GLYPH_META
    meta[0] = buf_w
    meta[1] = buf_h
    meta[3] = y
    meta[4] = fw
    meta[5] = fh
    meta[6] = bytes_per_row
    meta[8] = fg
    meta[9] = -1 if bg is None else bg

    cx = x
    for ch in string:
        code = ord(ch)
        if first <= code <= last:
            meta[2] = cx
            # An offset into the whole table, rather than slicing the glyph
            # out: the slice was a fresh memoryview per character.
            meta[7] = (code - first) * bytes_per_glyph
            _glyph16(buf, font_data, meta)
        # An undefined character just advances the width.
        cx += fw


def _buf_text_halo(buf, font, string, x, y, fg, halo, buf_w, buf_h):
    """
    Text with a 1px halo instead of an opaque backing rectangle.

    Labels are drawn after the traces, so a line crossing one would
    otherwise tangle with the glyph strokes and leave neither readable. A
    halo touches only the pixels immediately around each stroke - four
    extra glyph draws (offset up/down/left/right, transparent background)
    before the real one - rather than blanking a whole rectangle that
    would sit there whether or not a trace was actually underneath.
    """
    for dx, dy in ((-1, 0), (1, 0), (0, -1), (0, 1)):
        _buf_text(buf, font, string, x + dx, y + dy, halo, None, buf_w, buf_h)
    _buf_text(buf, font, string, x, y, fg, None, buf_w, buf_h)

# =========================================================
# COLOUR HELPERS
# =========================================================
def lerp(a, b, t):
    return int(a + (b - a) * t)


def lerp_color(c1, c2, t):
    r = lerp((c1 >> 11) & 0x1F, (c2 >> 11) & 0x1F, t)
    g = lerp((c1 >> 5)  & 0x3F, (c2 >> 5)  & 0x3F, t)
    b = lerp(c1 & 0x1F,         c2 & 0x1F,         t)
    return (r << 11) | (g << 5) | b


def temp_color(temp):
    """Colour ramp for the coolant loop (roughly 20 .. 45 C)."""
    if temp <= 25:
        return COLD
    if temp <= 35:
        return lerp_color(COLD, NORMAL, (temp - 25) / 10)
    return lerp_color(NORMAL, HOT, min((temp - 35) / 10, 1.0))


def pc_temp_color(temp):
    """Colour ramp for silicon temperatures (roughly 40 .. 95 C)."""
    if temp <= 55:
        return NORMAL
    if temp <= 80:
        return lerp_color(NORMAL, HOT, (temp - 55) / 25)
    return lerp_color(HOT, CRIT, min((temp - 80) / 12, 1.0))



# =========================================================
# THERMISTOR
# =========================================================
def read_ntc_resistance():
    raw = 0
    for _ in range(ADC_OVERSAMPLE):
        raw += adc.read_u16()
    raw /= ADC_OVERSAMPLE
    vout = (raw / 65535) * VCC
    vout = max(0.001, min(VCC - 0.001, vout))
    return (R_FIXED * vout) / (VCC - vout)


def resistance_to_celsius(r):
    if r <= 0:
        return None
    inv_t = (1.0 / T0) + (1.0 / B) * math.log(r / R0)
    return (1.0 / inv_t) - 273.15


def read_temperature():
    return resistance_to_celsius(read_ntc_resistance())

# =========================================================
# PC LINK
#
# The Pico has no network, but it is plugged into the PC for power and
# MicroPython exposes that USB port as a serial console.  While this script is
# running it owns stdin, so the PC-side bridge can just write lines to it:
#
#     T,<cpu>,<gpu>,<pump>,<cpuW>,<gpuW>\n
#         e.g.  "T,64.5,71.0,1532,142,318"  or  "T,-,71.0,-,-,318"
#
# CPU and GPU are hotspot temperatures, pump is RPM off the CPU fan header,
# and the last two are CPU package power and GPU board power in watts.
# "-" (or anything unparsable) means that sensor is unavailable.  Trailing
# fields may be missing entirely, so an older bridge still works.
#
# The updater asks one question, so that an update with nothing to send need
# not stop this script to find that out. A match means the device already
# holds those files, and it is left running - the graphs live in RAM, and a
# soft reset would empty them.
#
#     ?M      ->   M,<sha256 of the deploy record>   or   M,-
# =========================================================
DEPLOY_RECORD = "/.pico-deploy"

_rx_line = ""
_stdin_poll = None

try:
    import select
    _stdin_poll = select.poll()
    _stdin_poll.register(sys.stdin, select.POLLIN)
except Exception:
    _stdin_poll = None      # no serial link available; the rest still works


def _parse_field(parts, index, lo, hi):
    if len(parts) <= index:
        return None
    try:
        v = float(parts[index])
    except ValueError:
        return None
    if v < lo or v > hi:
        return None
    return v


_deploy_id = None


def _hash_deploy_record():
    """
    sha256 of the deploy record, hex, or "-" if it cannot be read.

    Only the PC ever interprets this; the device just reports what it has.
    """
    try:
        import hashlib
        import binascii
    except ImportError:
        return "-"
    try:
        h = hashlib.sha256()
        f = open(DEPLOY_RECORD, "rb")
        try:
            buf = bytearray(256)
            mv = memoryview(buf)
            while True:
                n = f.readinto(buf)
                if not n:
                    break
                h.update(mv[:n])
        finally:
            f.close()
        return binascii.hexlify(h.digest()).decode()
    except (OSError, AttributeError):
        return "-"


def _handle_query(line):
    """
    Answer the updater.  Computed once and kept: the record can only change
    via the REPL, and reaching the REPL stops this script anyway.
    """
    global _deploy_id
    if line[1:2].upper() != "M":
        return
    if _deploy_id is None:
        _deploy_id = _hash_deploy_record()
    sys.stdout.write("M," + _deploy_id + "\n")


def _handle_line(line):
    global pc_cpu, pc_gpu, pc_pump, pc_cpu_w, pc_gpu_w, pc_last_ms, _idle_ms
    if line[:1] == "?":
        _handle_query(line)
        return
    parts = line.replace(",", " ").split()
    if len(parts) < 2 or parts[0].upper() != "T":
        return
    pc_cpu = _parse_field(parts, 1, -50, 150)
    pc_gpu = _parse_field(parts, 2, -50, 150)
    pc_pump = _parse_field(parts, 3, 0, 20000)
    pc_cpu_w = _parse_field(parts, 4, 0, 2000)
    pc_gpu_w = _parse_field(parts, 5, 0, 2000)
    pc_last_ms = time.ticks_ms()
    _idle_ms = 0


def poll_pc_link():
    """Drain whatever is waiting on USB CDC and parse complete lines."""
    global _rx_line
    if _stdin_poll is None:
        return
    budget = 256
    while budget > 0 and _stdin_poll.poll(0):
        ch = sys.stdin.read(1)
        if not ch:
            break
        budget -= 1
        if ch == "\n" or ch == "\r":
            if _rx_line:
                try:
                    _handle_line(_rx_line)
                except Exception:
                    pass
                _rx_line = ""
        elif len(_rx_line) < 48:
            _rx_line += ch
        else:
            _rx_line = ""     # runaway line, resync


def pc_link_fresh():
    if pc_last_ms is None:
        return False
    return time.ticks_diff(time.ticks_ms(), pc_last_ms) < PC_TIMEOUT_MS

# =========================================================
# STANDBY, ZOOM AND SLEEP-NOW
#
# Not for burn-in: this is an IPS LCD, where that is not a failure mode. The
# backlight is what ages, and it is most of what the board draws, so there is
# no reason to run it while the PC it reports on is asleep.
#
# Sampling carries on while dark, so waking shows a real 30 minutes of history
# rather than an empty graph.
#
# Two of the four buttons (`_key_confirmed[0]` and `[1]`) do something once
# the panel is actually lit; the other two only ever wake it, same as
# before. Which physical corner is which key is a property of the board
# and this display's rotation, not of `tft_buttons.py`'s own comments -
# those describe a different program's orientation, not this one's; do not
# trust them here. While dark, all four are equivalent - any of them wakes
# the panel, and the two with a job are read again once it is up, so the
# press that woke it does not also count as that job's first press.
# =========================================================
try:
    import st7789.config.tft_buttons as tft_buttons
    _wake_keys = tuple(getattr(tft_buttons.Buttons(), "key%d" % i) for i in range(4))
except Exception:
    _wake_keys = ()          # no buttons wired; the PC link still wakes it

# Two consecutive ~250ms-spaced polls have to agree before a key's
# confirmed state moves - cheap tactile switches on a board like this can
# have marginal contact that flickers for a read or two even mid-hold, not
# just bounce right at the transition, and a single bad read must not be
# able to flip anything on its own.
_key_last_raw  = [False] * len(_wake_keys)
_key_confirmed = [False] * len(_wake_keys)

_standby = False
# Counted up by the main loop and zeroed whenever the PC speaks, rather than
# measured against ticks_ms.  time.ticks_diff is only meaningful for about six
# days, and a Pico on standby USB power outlives that easily.
_idle_ms = 0

_zoom_level = 0   # index into ZOOM_LEVELS/ZOOM_POINTS/ZOOM_LABELS; 0 = full window

# Set when standby was entered by the sleep-now button rather than the idle
# timeout - see leave_standby's use of it below.
_manual_sleep = False

# Level, not edge, is what a plain wake needs - holding a button is still a
# reason to stay awake. Zoom and sleep-now toggle something, so those need
# the rising edge instead, tracked here between ticks of the main loop.
_prev_wake_pressed = False
_prev_zoom_pressed = False
_prev_sleep_pressed = False


def _standby_due():
    """
    Whether to go dark.

    Never on a Pico that has not once heard from a PC: that is standalone use,
    where the coolant half works perfectly well on its own and blanking it
    would just look broken.
    """
    return (STANDBY_AFTER_MS > 0
            and pc_last_ms is not None
            and _idle_ms > STANDBY_AFTER_MS)


def _debounce_buttons():
    for i, key in enumerate(_wake_keys):
        raw = key.value() == 0
        if raw == _key_last_raw[i]:
            _key_confirmed[i] = raw
        _key_last_raw[i] = raw


def enter_standby(manual=False):
    global _standby, _manual_sleep
    _standby = True
    _manual_sleep = manual
    # Backlight first: the panel may show anything on its way into sleep, and
    # there is no reason for that to be visible.
    if tft.backlight is not None:
        tft.backlight.value(0)
    tft.sleep_mode(True)


def leave_standby():
    global _standby, _manual_sleep, _idle_ms, _last_temp_key, _last_pc_key
    _standby = False
    _manual_sleep = False
    _idle_ms = 0
    tft.sleep_mode(False)
    time.sleep_ms(120)                # the ST7789 wants this after SLPOUT
    # Nothing was drawn while dark and the panel's contents are not worth
    # trusting, so rebuild the screen before lighting it.
    draw_static_screen()
    _last_temp_key = None             # force the cached panels to redraw
    _last_pc_key = None
    if tft.backlight is not None:
        tft.backlight.value(1)


def _cycle_zoom():
    """
    Step to the next window in ZOOM_LEVELS, wrapping back to the full one,
    and redraw right away - the point of a button for this is that it
    reacts now, not up to GRAPH_PERIOD_MS later.
    """
    global _zoom_level
    _zoom_level = (_zoom_level + 1) % len(ZOOM_LEVELS)
    _push_live()
    try:
        draw_temp_graph()
        draw_power_graph()
    finally:
        _pop_live()


def _poll_buttons():
    """
    One tick's worth of wake/zoom/sleep-now button handling.

    Edge, not level, for all three: leaving standby needs a fresh press so
    the same press that woke the panel is not also read as zoom's or
    sleep-now's first press once the checks below run; zoom and sleep-now
    need it so holding the button does not repeat the action every tick.

    Manual sleep only leaves on a real press, never on `pc_link_fresh()`:
    the PC was already talking when the panel went dark on purpose, so it
    "still talking" a moment later says nothing. That check is for the
    idle-timeout case, where the PC being fresh again means it was gone -
    silent for the whole `STANDBY_AFTER_MS` - and has only just come back.
    """
    global _prev_wake_pressed, _prev_zoom_pressed, _prev_sleep_pressed

    _debounce_buttons()
    wake_now = any(_key_confirmed)
    zoom_now = len(_key_confirmed) > 0 and _key_confirmed[0]
    sleep_now = len(_key_confirmed) > 1 and _key_confirmed[1]

    if _standby:
        woke = wake_now and not _prev_wake_pressed
        if woke or (not _manual_sleep and pc_link_fresh()):
            leave_standby()
    elif _standby_due():
        enter_standby()
    elif zoom_now and not _prev_zoom_pressed:
        _cycle_zoom()
    elif sleep_now and not _prev_sleep_pressed:
        enter_standby(manual=True)

    _prev_wake_pressed = wake_now
    _prev_zoom_pressed = zoom_now
    _prev_sleep_pressed = sleep_now

# =========================================================
# STATIC SCREEN (drawn once directly to the display)
# =========================================================
def draw_static_screen():
    tft.fill(BG)
    tft.fill_rect(GRAPH_X - 4, G1_Y - 4, GRAPH_W + 8, G1_H + 8, PANEL)
    tft.fill_rect(GRAPH_X - 4, G2_Y - 4, GRAPH_W + 8, G2_H + 8, PANEL)
    tft.text(font_small, "COOLANT", LABEL_X, LABEL_Y, TEXT_DIM, BG)

# =========================================================
# HEADER: BIG COOLANT NUMBER
# =========================================================
_last_temp_key = None

def draw_temperature(temp):
    """Returns True if the buffer changed and needs blitting."""
    global _last_temp_key

    temp_text = "{:4.1f}".format(temp)
    if temp_text == _last_temp_key:
        return False
    _last_temp_key = temp_text

    _buf_fill(temp_buf, BG)
    color = temp_color(temp)

    _buf_text(temp_buf, font_big, temp_text, 0, TEMP_INSET_Y,
              color, BG, TEMP_BUF_W, TEMP_BUF_H)

    deg_x = len(temp_text) * font_big.WIDTH + 4
    _buf_text(temp_buf, font_med, "C", deg_x, 8, TEXT_DIM, BG,
              TEMP_BUF_W, TEMP_BUF_H)
    return True

# =========================================================
# HEADER: CPU / GPU / PUMP READOUT
# =========================================================
_last_pc_key = None

# Missing values are two dashes, right-aligned in their column so they land
# where the digits would. Filling the column instead ("----") crowds the label
# it sits next to, and a lone "-" strands itself against the panel edge.
NO_VALUE_3 = " --"
NO_VALUE_4 = "  --"


def _pc_row(label, y, label_color, value_text, value_color, value_unit=None,
            watt_text=None, watt_color=None):
    # The CPU and GPU labels carry the series colour - that is what identifies
    # the traces on the graphs below, so neither graph needs a legend.
    _buf_text(pc_buf, font_med, label, 0, y, label_color, BG,
              PC_BUF_W, PC_BUF_H)
    _buf_text(pc_buf, font_med, value_text, PC_COL_VALUE, y, value_color, BG,
              PC_BUF_W, PC_BUF_H)
    if value_unit is not None:
        _buf_text(pc_buf, font_small, value_unit, PC_COL_VALUE_UNIT,
                  y + PC_UNIT_DY, value_color, BG, PC_BUF_W, PC_BUF_H)
    if watt_text is not None:
        _buf_text(pc_buf, font_med, watt_text, PC_COL_WATTS, y, watt_color, BG,
                  PC_BUF_W, PC_BUF_H)
        _buf_text(pc_buf, font_small, "W", PC_COL_WATTS_UNIT,
                  y + PC_UNIT_DY, watt_color, BG, PC_BUF_W, PC_BUF_H)


def draw_pc_panel():
    """Returns True if the buffer changed and needs blitting."""
    global _last_pc_key

    live = pc_link_fresh()
    rpm = int(pc_pump) if pc_pump is not None else None
    key = (live,
           None if pc_cpu is None else int(pc_cpu),
           None if pc_gpu is None else int(pc_gpu),
           rpm,
           None if pc_cpu_w is None else int(pc_cpu_w),
           None if pc_gpu_w is None else int(pc_gpu_w))
    if key == _last_pc_key:
        return False
    _last_pc_key = key

    _buf_fill(pc_buf, BG)

    for label, color, temp, watts, y in (
            ("CPU", CPU_COL, pc_cpu, pc_cpu_w, PC_ROW_Y[0]),
            ("GPU", GPU_COL, pc_gpu, pc_gpu_w, PC_ROW_Y[1])):
        if live and temp is not None:
            _pc_row(label, y, color, "{:4.0f}".format(temp), pc_temp_color(temp),
                    "C",
                    "{:3.0f}".format(watts) if watts is not None else NO_VALUE_3,
                    TEXT_BRIGHT if watts is not None else STALE)
        else:
            _pc_row(label, y, STALE, NO_VALUE_4, STALE, "C", NO_VALUE_3, STALE)

    if live and rpm is not None:
        # A stalled pump is the one failure that actually matters here, so it
        # gets the alarm colour rather than blending in.
        _pc_row("PUMP", PC_ROW_Y[2], PUMP_COL, "{:4d}".format(rpm),
                CRIT if rpm < PUMP_MIN_RPM else TEXT_BRIGHT, "RPM")
    else:
        _pc_row("PUMP", PC_ROW_Y[2], STALE, NO_VALUE_4, STALE, "RPM")
    return True

# =========================================================
# GRAPH ENGINE
#
# The newest point sits on the right edge and history extends leftwards, so
# "now" is always in the same place regardless of window or zoom level.
# =========================================================
X_STEP  = (GRAPH_W - 1) / (HISTORY_POINTS - 1)             # full window; also the default below
X_STEPS = tuple((GRAPH_W - 1) / (p - 1) for p in ZOOM_POINTS)   # one per zoom level


def _graph_background(buf, bw, bh, nbytes, hlines=True):
    _buf_fill(buf, PANEL, nbytes)
    if hlines:
        for i in range(1, 4):                   # horizontal grid
            _buf_hline(buf, 0, (bh * i) // 4, bw, GRID, bw, bh)
    for i in range(1, 6):                       # six even divisions of the window
        _buf_vrun(buf, (bw - 1) * i // 6, 0, bh - 1, GRID, bw, bh)


def _series_range(series_list, min_span=2.0):
    """Common (tmin, span) across every series that has data."""
    lo = None
    hi = None
    for data in series_list:
        for v in data:
            if v != v:            # NaN marks a missing bucket
                continue
            if lo is None or v < lo:
                lo = v
            if hi is None or v > hi:
                hi = v
    if lo is None:
        return None, None
    span = max(min_span, hi - lo)
    mid = (lo + hi) / 2
    return mid - span / 2, span


def _latest(data):
    """Newest real (non-NaN) sample, for colouring things by its value."""
    for i in range(len(data) - 1, -1, -1):
        v = data[i]
        if v == v:              # not NaN - a real reading
            return v
    return None


# One column's y and colour, filled in by _plot_series and drawn by _plot_run.
_PLOT_GAP = 0xFFFF          # no data for this column; break the line
_PLOT_YS = array('H', bytearray(GRAPH_W * 2))
_PLOT_COLS = array('H', bytearray(GRAPH_W * 2))
_PLOT_META = array('i', bytearray(7 * 4))  # bw, bh, start_px, count, half_width, bg1, bg2


@micropython.viper
def _plot_run(buf: ptr16, ys: ptr16, cols: ptr16, meta: ptr32):
    """
    Draw a whole prepared series - one call rather than one per column, since
    the joining-up and the pixels are all integer work.
    """
    buf_w = int(meta[0])
    buf_h = int(meta[1])
    start = int(meta[2])
    count = int(meta[3])
    half = int(meta[4])

    prev = -1                       # y of the column before, -1 after a gap
    i = 0
    while i < count:
        py = int(ys[i])
        if py == 0xFFFF:
            prev = -1
        else:
            top = py - half
            bot = py + half
            if prev >= 0:           # span the step, so the line joins up
                if prev - half < top:
                    top = prev - half
                if prev + half > bot:
                    bot = prev + half
            if top < 0:
                top = 0
            if bot > buf_h - 1:
                bot = buf_h - 1
            color = int(cols[i])
            value = ((color & 0xFF) << 8) | ((color >> 8) & 0xFF)
            index = top * buf_w + start + i
            n = bot - top + 1
            while n > 0:
                buf[index] = value
                index = index + buf_w
                n = n - 1
            prev = py
        i = i + 1


@micropython.viper
def _plot_run_blend(buf: ptr16, ys: ptr16, cols: ptr16, meta: ptr32):
    """
    Same joined-line draw as `_plot_run`, for a series drawn on top of others
    that share its pixel rows - two independent scales share the same panel
    height, so a column can belong to more than one trace.

    A pixel that already holds plain panel background (`meta[5]`) or grid
    (`meta[6]`) draws solid, exactly as `_plot_run` would. Anything else is
    something another trace already put there. Rather than replace it
    outright, or split the colour evenly with it, the pixel is weighted 3:1
    toward this line - a single crossing still visibly blends into the
    other trace, but a run of columns where the two exactly coincide (a
    flat silicon reading under a flat coolant one, say) keeps reading as
    this line's own colour instead of settling into a flat, third colour
    that belongs to neither and reads as this line having stopped.
    """
    buf_w = int(meta[0])
    buf_h = int(meta[1])
    start = int(meta[2])
    count = int(meta[3])
    half = int(meta[4])
    # Stored pixels are byte-swapped (see `_fill16`); swap the two background
    # colours once here rather than un-swapping every pixel read below.
    bg1 = int(meta[5])
    bg1 = ((bg1 & 0xFF) << 8) | ((bg1 >> 8) & 0xFF)
    bg2 = int(meta[6])
    bg2 = ((bg2 & 0xFF) << 8) | ((bg2 >> 8) & 0xFF)

    prev = -1
    i = 0
    while i < count:
        py = int(ys[i])
        if py == 0xFFFF:
            prev = -1
        else:
            top = py - half
            bot = py + half
            if prev >= 0:
                if prev - half < top:
                    top = prev - half
                if prev + half > bot:
                    bot = prev + half
            if top < 0:
                top = 0
            if bot > buf_h - 1:
                bot = buf_h - 1
            color = int(cols[i])
            value = ((color & 0xFF) << 8) | ((color >> 8) & 0xFF)
            fr = (color >> 11) & 0x1F
            fg = (color >> 5) & 0x3F
            fb = color & 0x1F
            index = top * buf_w + start + i
            n = bot - top + 1
            while n > 0:
                existing = int(buf[index])
                if existing == bg1 or existing == bg2:
                    buf[index] = value
                else:
                    # 3:1 toward this line's colour - see the docstring for
                    # why an even split does not stay legible.
                    ex = ((existing & 0xFF) << 8) | ((existing >> 8) & 0xFF)
                    er = (ex >> 11) & 0x1F
                    eg = (ex >> 5) & 0x3F
                    eb = ex & 0x1F
                    nr = (fr * 3 + er) >> 2
                    ng = (fg * 3 + eg) >> 2
                    nb = (fb * 3 + eb) >> 2
                    blended = (nr << 11) | (ng << 5) | nb
                    buf[index] = ((blended & 0xFF) << 8) | ((blended >> 8) & 0xFF)
                index = index + buf_w
                n = n - 1
            prev = py
        i = i + 1


def _plot_series(buf, bw, bh, data, color, tmin, span, half_width=1, blend_bg=None,
                 x_step=X_STEP):
    """
    `color` is either a 565 value or a function of the sample value.

    `blend_bg` is `(panel_color, grid_color)` for a series drawn where
    another one may already be - see `_plot_run_blend`. `None` (the default)
    draws solid, as every series but the coolant one does.

    `x_step` is pixels per bucket for the window currently on screen - the
    full window by default, or one of `X_STEPS` when a caller wants a
    shorter trailing slice of history spread across the full graph width
    instead of bunched at the right edge.
    """
    tinted = callable(color)
    n = len(data)
    if n < 1:
        return

    x_first = (bw - 1) - (n - 1) * x_step       # x of data[0]
    start_px = int(x_first) if x_first > 0 else 0
    inv_span = 1.0 / span
    usable = bh - 1

    # Interpolation is floating point, which viper does not do, so the values
    # are worked out here and the drawing handed over in one go.
    ys = _PLOT_YS
    cols = _PLOT_COLS
    count = 0

    for px in range(start_px, bw):
        pos = (px - x_first) / x_step
        if pos < 0:
            pos = 0.0
        i = int(pos)
        if i >= n - 1:
            val = data[n - 1]
            if val != val:                # NaN marks a missing bucket
                val = None
        else:
            a = data[i]
            b = data[i + 1]
            if a != a or b != b:          # NaN marks a missing bucket
                val = None
            else:
                val = a + (b - a) * (pos - i)

        if val is None:                          # gap in the data
            ys[count] = _PLOT_GAP
        else:
            f = (val - tmin) * inv_span
            if f < 0.04:
                f = 0.04
            elif f > 0.96:
                f = 0.96
            ys[count] = usable - int(f * usable)
            cols[count] = color(val) if tinted else color
        count += 1

    meta = _PLOT_META
    meta[0] = bw
    meta[1] = bh
    meta[2] = start_px
    meta[3] = count
    meta[4] = half_width
    if blend_bg is None:
        _plot_run(buf, ys, cols, meta)
    else:
        meta[5], meta[6] = blend_bg
        _plot_run_blend(buf, ys, cols, meta)


def _scale_labels(buf, bw, bh, tmin, span, unit, color=TEXT_DIM, right=False):
    """
    High and low value of a scale, at the top and bottom edge respectively.

    The label doubles as the scale's axis this way: the number at a given
    edge is what the trace touching that edge means, with no separate
    lookup between a number and the height it describes.
    """
    hi_text = "{:.0f}{}".format(tmin + span, unit)
    lo_text = "{:.0f}{}".format(tmin, unit)
    hi_x = (bw - 2 - len(hi_text) * font_small.WIDTH) if right else 2
    lo_x = (bw - 2 - len(lo_text) * font_small.WIDTH) if right else 2
    _buf_text_halo(buf, font_small, hi_text, hi_x, 1, color, PANEL, bw, bh)
    _buf_text_halo(buf, font_small, lo_text, lo_x, bh - font_small.HEIGHT - 1,
                   color, PANEL, bw, bh)


def _graph_window():
    """(point count, x-step, series to read) for the window on screen."""
    series = ALL_FINE_SERIES if ZOOM_FINE[_zoom_level] else ALL_SERIES
    return ZOOM_POINTS[_zoom_level], X_STEPS[_zoom_level], series


def draw_temp_graph():
    """
    Coolant, CPU and GPU on one panel, on two scales.

    Coolant moves across a couple of degrees while the silicon moves across
    tens, so a single shared range would squash the coolant trace into a line
    a few pixels tall - the one series you most want to read. Instead the
    silicon is labelled down the left and coolant down the right in its own
    colour, and each uses the full panel height. The time axis is shared, so
    load and its effect on the water line up vertically.

    Three traces on one panel gets busy, so everything except coolant is
    turned down: the silicon is hairline and muted, and the horizontal grid is
    dropped - with two scales in play those lines corresponded to neither, so
    they were decoration that happened to look like information.

    Coolant is the same weight as the other two rather than a thicker line,
    so where it crosses a silicon trace it blends into it instead of
    erasing it - a thin line keeps that overlap short to begin with.
    """
    bw, bh = GRAPH_W, G1_H
    _graph_background(graph_buf, bw, bh, _G1_BYTES, hlines=False)

    points, step, series = _graph_window()
    loop_series, cpu_series, gpu_series = series[0], series[1], series[2]
    cpu_view = cpu_series.hist[-points:]
    gpu_view = gpu_series.hist[-points:]
    loop_view = loop_series.hist[-points:]

    si_min, si_span = _series_range((cpu_view, gpu_view))
    lo_min, lo_span = _series_range((loop_view,))

    # Silicon first, so coolant lands on top of it where they cross.
    if si_min is not None:
        _plot_series(graph_buf, bw, bh, gpu_view, GPU_MUTED,
                     si_min, si_span, half_width=0, x_step=step)
        _plot_series(graph_buf, bw, bh, cpu_view, CPU_MUTED,
                     si_min, si_span, half_width=0, x_step=step)
    if lo_min is not None:
        _plot_series(graph_buf, bw, bh, loop_view, temp_color,
                     lo_min, lo_span, half_width=0, blend_bg=(PANEL, GRID),
                     x_step=step)

    # Labels last, after every trace - drawing order is what keeps the
    # halo legible, since a line plotted on top of it would poke straight
    # through the gaps between strokes.
    if si_min is not None:
        _scale_labels(graph_buf, bw, bh, si_min, si_span, "C")
    if lo_min is not None:
        newest = _latest(loop_view)
        _scale_labels(graph_buf, bw, bh, lo_min, lo_span, "C",
                     temp_color(newest) if newest is not None else NORMAL,
                     right=True)
    tft.blit_buffer(memoryview(graph_buf)[:_G1_BYTES], GRAPH_X, G1_Y, bw, bh)


def draw_power_graph():
    """CPU and GPU power on a shared scale - both are watts, so they compare."""
    bw, bh = GRAPH_W, G2_H
    _graph_background(graph_buf, bw, bh, _G2_BYTES)

    points, step, series = _graph_window()
    cpu_w_series, gpu_w_series = series[3], series[4]
    cpu_w_view = cpu_w_series.hist[-points:]
    gpu_w_view = gpu_w_series.hist[-points:]

    # A 20 W floor stops idle jitter being amplified to fill the panel.
    pmin, pspan = _series_range((cpu_w_view, gpu_w_view), min_span=20.0)
    if pmin is not None:
        # Same weight and muted colours as the panel above: CPU is CPU on both,
        # and hairlines carry a spiky series like power better than thick ones.
        _plot_series(graph_buf, bw, bh, gpu_w_view, GPU_MUTED,
                     pmin, pspan, half_width=0, x_step=step)
        _plot_series(graph_buf, bw, bh, cpu_w_view, CPU_MUTED,
                     pmin, pspan, half_width=0, x_step=step)
        _scale_labels(graph_buf, bw, bh, pmin, pspan, "W")

    # Which window is on screen is otherwise only visible as a jump in
    # scale and how spread out the traces are - easy to not notice, and
    # then forget the graphs are zoomed in at all.
    window_text = ZOOM_LABELS[_zoom_level]
    window_x = bw - 2 - len(window_text) * font_small.WIDTH
    _buf_text_halo(graph_buf, font_small, window_text, window_x, 1,
                   TEXT_DIM, PANEL, bw, bh)
    tft.blit_buffer(memoryview(graph_buf)[:_G2_BYTES], GRAPH_X, G2_Y, bw, bh)

# =========================================================
# HISTORY BOOKKEEPING
# =========================================================
def accumulate(loop_temp):
    loop_s.add(loop_temp)
    loop_fine.add(loop_temp)
    # Only fold PC values in while the link is alive, so a dead bridge leaves
    # a gap in the history rather than a flat line at the last known value.
    if pc_link_fresh():
        cpu_s.add(pc_cpu)
        gpu_s.add(pc_gpu)
        cpu_w_s.add(pc_cpu_w)
        gpu_w_s.add(pc_gpu_w)
        cpu_fine.add(pc_cpu)
        gpu_fine.add(pc_gpu)
        cpu_w_fine.add(pc_cpu_w)
        gpu_w_fine.add(pc_gpu_w)


def close_bucket():
    for series in ALL_SERIES:
        series.close()


def close_fine_bucket():
    for series in ALL_FINE_SERIES:
        series.close()


def _push_live():
    for series in ALL_SERIES:
        series.push_live()
    for series in ALL_FINE_SERIES:
        series.push_live()


def _pop_live():
    for series in ALL_SERIES:
        series.pop_live()
    for series in ALL_FINE_SERIES:
        series.pop_live()


# =========================================================
# MAIN
# =========================================================
def main():
    global _idle_ms

    draw_static_screen()

    shown = None

    now = time.ticks_ms()
    last_tick = now
    next_bucket_ms = time.ticks_add(now, BUCKET_MS)
    next_fine_bucket_ms = time.ticks_add(now, FINE_BUCKET_MS)
    next_graph_ms = now

    while True:
        poll_pc_link()

        now = time.ticks_ms()
        # Between two turns of this loop, so always a small number - unlike a
        # difference against whenever the PC last spoke, which may be days.
        _idle_ms += time.ticks_diff(now, last_tick)
        last_tick = now

        _poll_buttons()

        temp = read_temperature()
        if temp is not None:
            shown = temp if shown is None else shown + EMA_ALPHA * (temp - shown)
            accumulate(temp)

            if not _standby and draw_temperature(shown):
                tft.blit_buffer(temp_buf, TEMP_BUF_SCREEN_X, TEMP_BUF_SCREEN_Y,
                                TEMP_BUF_W, TEMP_BUF_H)

        if not _standby and draw_pc_panel():
            tft.blit_buffer(pc_buf, PC_X, PC_Y, PC_BUF_W, PC_BUF_H)

        now = time.ticks_ms()
        if time.ticks_diff(now, next_bucket_ms) >= 0:
            close_bucket()
            next_bucket_ms = time.ticks_add(next_bucket_ms, BUCKET_MS)
            if time.ticks_diff(now, next_bucket_ms) >= 0:   # fell behind
                next_bucket_ms = time.ticks_add(now, BUCKET_MS)

        if time.ticks_diff(now, next_fine_bucket_ms) >= 0:
            close_fine_bucket()
            next_fine_bucket_ms = time.ticks_add(next_fine_bucket_ms, FINE_BUCKET_MS)
            if time.ticks_diff(now, next_fine_bucket_ms) >= 0:   # fell behind
                next_fine_bucket_ms = time.ticks_add(now, FINE_BUCKET_MS)
        # The timer is advanced even while dark, so it cannot fall so far
        # behind that ticks_diff stops meaning anything.
        if time.ticks_diff(now, next_graph_ms) >= 0:
            if not _standby:
                _push_live()
                try:
                    draw_temp_graph()
                    draw_power_graph()
                finally:
                    _pop_live()
            next_graph_ms = time.ticks_add(time.ticks_ms(), GRAPH_PERIOD_MS)

        time.sleep(SAMPLE_PERIOD)


# MicroPython names the entry-point script "__main__", same as CPython, so
# this runs unchanged on the device. It also makes the module importable
# (for tooling such as tools/render_preview.py) without falling into the
# event loop.
if __name__ == "__main__":
    main()
