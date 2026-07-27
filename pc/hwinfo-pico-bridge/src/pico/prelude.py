# Helpers the updater runs on the Pico, sent once per session over the raw
# REPL and then called by `pico::mod`. Globals persist between calls, which is
# what lets a file be opened once and written in chunks.
#
# This is a real file rather than a string literal in the Rust source so that
# CI can put a Python parser over it: a syntax error here would otherwise only
# show up as a traceback from a device someone is trying to update.
import gc
import os
try:
    import binascii as _ba
except ImportError:
    import ubinascii as _ba

# Whatever was running got interrupted mid-flight, and its garbage is still
# on the heap. A Pico has a couple of hundred KB to play with and a transfer
# needs a few KB per chunk, so reclaiming it up front is the difference
# between a deploy working and a MemoryError part way through a file.
gc.collect()
try:
    import hashlib as _hl
    _hl.sha256
except (ImportError, AttributeError):
    # No hashlib in this build: uploads can only be checked by size.
    _hl = None


def _mkdir(p):
    try:
        os.mkdir(p)
    except OSError:
        pass


def _walk(d):
    for n in os.listdir(d):
        p = (d + '/' + n) if d != '/' else ('/' + n)
        st = os.stat(p)
        if st[0] & 0x4000:
            _walk(p)
        else:
            print(st[6], p)


def _cat(p):
    try:
        print(open(p).read())
    except OSError:
        pass


def _sha(p):
    if _hl is None:
        return '-'
    h = _hl.sha256()
    f = open(p, 'rb')
    b = bytearray(512)
    m = memoryview(b)
    while True:
        n = f.readinto(b)
        if not n:
            break
        h.update(m[:n])
    f.close()
    return _ba.hexlify(h.digest()).decode()


def _check(p):
    print(_sha(p), os.stat(p)[6])


def _rm(p):
    try:
        os.remove(p)
    except OSError:
        pass


def _space():
    # Total and free bytes of the device filesystem. Zeroes mean this port
    # cannot say, which the PC treats as "do not report it".
    try:
        s = os.statvfs('/')
        print(s[1] * s[2], s[1] * s[3])
    except (OSError, AttributeError, IndexError):
        print(0, 0)
