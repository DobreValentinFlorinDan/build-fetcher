# pow.bin for crates/core/tests/jsmath.rs: the inputs of an Electron-made pow.bin (parity/math-fixtures.mjs), each
# result replaced by the correctly rounded power from 400-bit arithmetic (mpmath). Special cases (zeros, infinities,
# NaN, a negative base with a fractional exponent) keep JavaScript's answer.
#
#   python3 parity/pow-fixtures.py <electron pow.bin> <out pow.bin>
import math, struct, sys
import mpmath as mp

mp.mp.prec = 400
raw = open(sys.argv[1], 'rb').read()
rows = struct.iter_unpack('<3d', raw)
out = bytearray()
changed = 0
TOP = mp.mpf(2) ** 1024 - mp.mpf(2) ** 970  # at or above this a result rounds to infinity
for x, y, e in rows:
    r = e
    if math.isfinite(x) and math.isfinite(y) and x != 0 and y != 0 and (x > 0 or y == int(y)):
        t = mp.power(mp.mpf(abs(x)), mp.mpf(y))
        r = math.inf if t >= TOP else float(t)
        if x < 0 and int(y) % 2 == 1:
            r = -r
        if struct.pack('<d', r) != struct.pack('<d', e):
            changed += 1
    out += struct.pack('<3d', x, y, r)
open(sys.argv[2], 'wb').write(out)
print(f'{len(out) // 24} rows, {changed} differ from the C library\'s result in the input file')
