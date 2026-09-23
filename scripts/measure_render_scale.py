"""Measure the true board-mm -> image-px scale of a `kicad-cli pcb render`.

Decodes the PNG (truecolour, non-interlaced), finds the greenish soldermask
pixels, and compares the board's measured pixel span against the Edge.Cuts
rectangle in the .kicad_pcb — so we can check what guide.rs's render_scale
predicts against what KiCad actually drew.
"""

import math
import re
import struct
import sys
import zlib


def decode(path):
    d = open(path, "rb").read()
    i, idat, bpp = 8, b"", 3
    w = h = 0
    while i < len(d):
        ln = struct.unpack(">I", d[i : i + 4])[0]
        typ, data = d[i + 4 : i + 8], d[i + 8 : i + 8 + ln]
        i += 12 + ln
        if typ == b"IHDR":
            w, h, _, ct = struct.unpack(">IIBB", data[:10])
            bpp = 3 if ct == 2 else 4
        elif typ == b"IDAT":
            idat += data
    raw = zlib.decompress(idat)
    stride, rows, prev, p = w * bpp, [], bytearray(w * bpp), 0
    for _ in range(h):
        f = raw[p]
        p += 1
        line = bytearray(raw[p : p + stride])
        p += stride
        if f == 1:
            for x in range(bpp, stride):
                line[x] = (line[x] + line[x - bpp]) & 255
        elif f == 2:
            for x in range(stride):
                line[x] = (line[x] + prev[x]) & 255
        elif f == 3:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                line[x] = (line[x] + ((a + prev[x]) >> 1)) & 255
        elif f == 4:
            for x in range(stride):
                a = line[x - bpp] if x >= bpp else 0
                c = prev[x - bpp] if x >= bpp else 0
                b = prev[x]
                pa, pb, pc = abs(b - c), abs(a - c), abs(a + b - 2 * c)
                pr = a if (pa <= pb and pa <= pc) else (b if pb <= pc else c)
                line[x] = (line[x] + pr) & 255
        rows.append(line)
        prev = line
    return w, h, bpp, rows


def board_box(w, h, bpp, rows):
    xs, ys = [], []
    for y in range(0, h, 2):
        r = rows[y]
        for x in range(0, w, 2):
            R, G, B = r[x * bpp], r[x * bpp + 1], r[x * bpp + 2]
            if G > R + 12 and G > B + 12:
                xs.append(x)
                ys.append(y)
    return min(xs), min(ys), max(xs), max(ys)


def outline(pcb):
    s = open(pcb).read()
    for m in re.finditer(r"\(gr_rect\s+\(start ([-\d.]+) ([-\d.]+)\)\s+\(end ([-\d.]+) ([-\d.]+)\)(.{0,200})", s, re.S):
        if "Edge.Cuts" in m.group(5):
            x0, y0, x1, y1 = (float(m.group(i)) for i in range(1, 5))
            return abs(x1 - x0), abs(y1 - y0)
    raise SystemExit("no Edge.Cuts rect")


png, pcb = sys.argv[1], sys.argv[2]
w, h, bpp, rows = decode(png)
bx0, by0, bx1, by1 = board_box(w, h, bpp, rows)
mw, mh = outline(pcb)
sx, sy = (bx1 - bx0) / mw, (by1 - by0) / mh
pred = min(w, h) / math.hypot(mw, mh)
real = (sx + sy) / 2
print(
    f"{pcb.split('/')[-1]:28} board {mw:6.2f}x{mh:6.2f}mm  aspect {mw/mh:5.3f}  "
    f"real {real:7.4f}  predicted {pred:7.4f}  ratio {real/pred:6.4f}  "
    f"centre ({(bx0+bx1)/2:.0f},{(by0+by1)/2:.0f}) vs ({w/2:.0f},{h/2:.0f})"
)
