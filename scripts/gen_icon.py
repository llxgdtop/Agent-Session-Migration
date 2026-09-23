#!/usr/bin/env python3
"""Generate the Agent Session Hub application icon.

Design: a rounded-square gradient tile sized to the macOS guideline
(the tile fills ~82% of the canvas so the icon does not render larger
than neighboring icons in the Dock) with a white two-arrow swap glyph:
two horizontal round-capped strokes with solid arrowheads pointing
right (top) and left (bottom), rotationally symmetric about the center.

Everything is pure-stdlib Python (math/zlib/struct/array) and
deterministic: repeated runs produce byte-identical outputs.

Outputs (relative to the repo root):
  assets/icon-1024.png   master artwork (RGBA PNG)
  assets/icon-256.rgba   raw straight-alpha RGBA for the eframe window icon
  assets/icon.icns       macOS iconset bundle (needs sips + iconutil)

Usage: python3 scripts/gen_icon.py   (any cwd; ~15-25 s)
"""

import math
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import zlib
from array import array

MASTER = 1024             # master output size
WINDOW = 256              # window-icon output size
SS = 4                    # supersampling factor per axis (4096 canvas)

GRAD_TOP = (0x3B, 0x5B, 0xDB)  # deep indigo, top of the gradient
GRAD_BOT = (0x2F, 0x40, 0x9E)  # deep blue-purple, bottom of the gradient

# --- Background tile: rounded square occupying 82% of the canvas ---------
BG_INSET = 0.09           # transparent margin on every side
BG_CORNER = 0.185         # corner radius (fraction of the full side)

# --- Swap glyph (fractions of the full side, center = 0.5) ---------------
# Two rotationally symmetric arrows. Each is a round-capped horizontal
# stroke plus a triangular head at its leading end.
STROKE_HW = 0.046         # stroke half width
ARROW_TOP_Y = 0.385       # center line of the right-pointing arrow
ARROW_BOT_Y = 0.615       # center line of the left-pointing arrow
# Top arrow (points right): round-capped tail at 0.315, head tip 0.725.
# Bottom arrow (points left) mirrors it through the canvas center.
TOP_TAIL_X = 0.300
TOP_TIP_X = 0.740
HEAD_BASE_BACK = 0.105    # head base sits this far behind the tip
HEAD_HW = 0.075           # arrowhead half width

# Derived glyph bounding box (with a small safety pad for the round caps).
GLYPH_BBOX = (0.24, 0.32, 0.76, 0.68)


# ---------------------------------------------------------------------------
# Glyph geometry
# ---------------------------------------------------------------------------

def build_glyph(scale):
    """Precompute glyph primitives at `scale` subpixels.

    Returns (strokes, heads):
      strokes -- list of (y, x0, x1, hw2): a point is inside the stroke
                 when its squared distance to the segment (with round
                 caps) is <= hw2. Horizontal segments make this cheap.
      heads   -- per arrowhead three edge functions (A, B, C) that are
                 >= 0 inside the triangle.
    """
    # (axis_y, tail_x, tip_x) in unit fractions; the head points toward
    # the tip, the round cap sits on the tail. Bottom mirrors top through
    # the canvas center (rotationally symmetric swap motif).
    arrows = (
        (ARROW_TOP_Y, TOP_TAIL_X, TOP_TIP_X),
        (ARROW_BOT_Y, 1.0 - TOP_TAIL_X, 1.0 - TOP_TIP_X),
    )

    strokes = []
    heads = []
    for (ay_frac, tail_frac, tip_frac) in arrows:
        ay = ay_frac * scale
        direction = 1.0 if tip_frac > tail_frac else -1.0
        base_frac = tip_frac - direction * HEAD_BASE_BACK
        x0, x1 = sorted((tail_frac * scale, base_frac * scale))
        strokes.append((ay, x0, x1, (STROKE_HW * scale) ** 2))

        tip = tip_frac * scale
        base = base_frac * scale
        hw = HEAD_HW * scale
        verts = (
            (tip, ay),
            (base, ay - hw),
            (base, ay + hw),
        )
        edges = []
        gx = sum(v[0] for v in verts) / 3.0
        gy = sum(v[1] for v in verts) / 3.0
        for k in range(3):
            xa, ya = verts[k]
            xb, yb = verts[(k + 1) % 3]
            a, b, c = ya - yb, xb - xa, xa * yb - xb * ya
            if a * gx + b * gy + c < 0.0:
                a, b, c = -a, -b, -c
            edges.append((a, b, c))
        heads.append(edges)
    return strokes, heads


# ---------------------------------------------------------------------------
# Renderer
# ---------------------------------------------------------------------------

def render(size, ss):
    """Rasterize the icon on a size*ss supersampled canvas.

    Returns the per-output-pixel channel SUMS (array('H'), 4 channels,
    straight color weighted by coverage) for the `size` output; the
    caller divides by the sample count.
    """
    canvas = size * ss
    n = ss * ss  # samples per output pixel
    c = 0.5 * canvas
    # Background tile geometry (82% of the canvas, centered).
    half = 0.5 * canvas * (1.0 - 2.0 * BG_INSET)
    corner = BG_CORNER * canvas
    inner = half - corner  # inner (straight-edge) rect half extent

    strokes, heads = build_glyph(canvas)
    bx0, by0, bx1, by1 = (GLYPH_BBOX[0] * canvas, GLYPH_BBOX[1] * canvas,
                          GLYPH_BBOX[2] * canvas, GLYPH_BBOX[3] * canvas)

    acc = array("H", [0]) * (size * size * 4)
    shift = ss.bit_length() - 1  # log2(ss): subpixel -> output pixel

    for sy in range(canvas):
        y = sy + 0.5
        dy = y - c
        # Rounded-square half width for this row (exact union of the
        # straight band and the corner discs).
        qy = abs(dy) - inner
        if qy <= 0.0:
            w_row = half
        elif qy <= corner:
            w_row = inner + math.sqrt(corner * corner - qy * qy)
        else:
            continue  # fully transparent row, nothing to accumulate
        # Vertical gradient color for this row (integer channels).
        t = y / canvas
        gr = int(GRAD_TOP[0] + (GRAD_BOT[0] - GRAD_TOP[0]) * t + 0.5)
        gg = int(GRAD_TOP[1] + (GRAD_BOT[1] - GRAD_TOP[1]) * t + 0.5)
        gb = int(GRAD_TOP[2] + (GRAD_BOT[2] - GRAD_TOP[2]) * t + 0.5)
        wr = int(255 - gr)  # glyph deltas: white overrides the gradient
        wg = int(255 - gg)
        wb = int(255 - gb)

        row_base = (sy >> shift) * size * 4

        # --- Pass 1: background over the covered interval -------------
        lo = max(0, int(math.ceil(c - w_row - 0.5)))
        hi = min(canvas - 1, int(math.floor(c + w_row - 0.5)))
        for sx in range(lo, hi + 1):
            j = row_base + ((sx >> shift) << 2)
            acc[j] += gr
            acc[j + 1] += gg
            acc[j + 2] += gb
            acc[j + 3] += 1

        # --- Pass 2: glyph rows only -----------------------------------
        if not (by0 <= y <= by1):
            continue
        glo = max(lo, int(math.ceil(bx0 - 0.5)))
        ghi = min(hi, int(math.floor(bx1 - 0.5)))
        # Pre-select strokes touching this row; heads stay unfiltered
        # (the edge test itself is cheap and triangles span few rows).
        row_strokes = [s for s in strokes
                       if abs(y - s[0]) <= STROKE_HW * canvas + 1.0]
        for sx in range(glo, ghi + 1):
            x = sx + 0.5
            hit = False
            for (ay, x0, x1, hw2) in row_strokes:
                dx = x - x0 if x < x0 else (x - x1 if x > x1 else 0.0)
                ddy = y - ay
                if dx * dx + ddy * ddy <= hw2:
                    hit = True
                    break
            if not hit:
                for edges in heads:
                    if (edges[0][0] * x + edges[0][1] * y + edges[0][2] >= 0.0
                            and edges[1][0] * x + edges[1][1] * y
                            + edges[1][2] >= 0.0
                            and edges[2][0] * x + edges[2][1] * y
                            + edges[2][2] >= 0.0):
                        hit = True
                        break
            if hit:
                j = row_base + ((sx >> shift) << 2)
                acc[j] += wr
                acc[j + 1] += wg
                acc[j + 2] += wb
    return acc, n


def downsample_sums(acc, size, factor):
    """Sum `factor`x`factor` blocks of per-pixel sums (a further box
    filter on the supersampled data, exact because sums compose)."""
    out_size = size // factor
    out = array("H", [0]) * (out_size * out_size * 4)
    for y in range(out_size):
        src_row = (y * factor) * size * 4
        dst_row = y * out_size * 4
        for x in range(out_size):
            src = src_row + (x * factor) * 4
            dst = dst_row + x * 4
            for yy in range(factor):
                p = src + yy * size * 4
                for xx in range(factor):
                    q = p + xx * 4
                    out[dst] += acc[q]
                    out[dst + 1] += acc[q + 1]
                    out[dst + 2] += acc[q + 2]
                    out[dst + 3] += acc[q + 3]
    return out


def finalize(acc, n, size):
    """Convert per-pixel channel sums to straight-alpha RGBA bytes with
    round-half-up integer arithmetic (deterministic)."""
    out = bytearray(size * size * 4)
    for i in range(size * size):
        j = i * 4
        sa = acc[j + 3]
        if sa == 0:
            continue
        out[j + 3] = (sa * 510 + n) // (2 * n)
        out[j] = (acc[j] * 2 + sa) // (2 * sa)
        out[j + 1] = (acc[j + 1] * 2 + sa) // (2 * sa)
        out[j + 2] = (acc[j + 2] * 2 + sa) // (2 * sa)
    return out


# ---------------------------------------------------------------------------
# Encoders
# ---------------------------------------------------------------------------

def png_bytes(width, height, rgba):
    """Encode straight-alpha RGBA bytes as a PNG (filter 0, zlib level 9)."""
    stride = width * 4
    raw = bytearray()
    for off in range(0, height * stride, stride):
        raw.append(0)  # filter type None per scanline
        raw += rgba[off : off + stride]

    def chunk(tag, payload):
        body = tag + payload
        return (
            struct.pack(">I", len(payload))
            + body
            + struct.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def build_icns(master_png, out_path):
    """Derive iconset sizes with sips and pack them with iconutil."""
    if not (shutil.which("sips") and shutil.which("iconutil")):
        print("sips/iconutil not found; skipping icon.icns")
        return False
    sizes = (
        ("icon_16x16.png", 16),
        ("icon_16x16@2x.png", 32),
        ("icon_32x32.png", 32),
        ("icon_32x32@2x.png", 64),
        ("icon_128x128.png", 128),
        ("icon_128x128@2x.png", 256),
        ("icon_256x256.png", 256),
        ("icon_256x256@2x.png", 512),
        ("icon_512x512.png", 512),
        ("icon_512x512@2x.png", 1024),
    )
    tmp = tempfile.mkdtemp(prefix="ash-iconset-", suffix=".iconset")
    try:
        for name, px in sizes:
            subprocess.run(
                ["sips", "-z", str(px), str(px), master_png,
                 "--out", os.path.join(tmp, name)],
                check=True,
                capture_output=True,
            )
        subprocess.run(
            ["iconutil", "-c", "icns", tmp, "-o", out_path],
            check=True,
            capture_output=True,
        )
    finally:
        shutil.rmtree(tmp, ignore_errors=True)
    return True


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

def main():
    repo = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
    assets = os.path.join(repo, "assets")
    os.makedirs(assets, exist_ok=True)

    acc, n = render(MASTER, SS)
    master_rgba = finalize(acc, n, MASTER)
    png_path = os.path.join(assets, "icon-1024.png")
    with open(png_path, "wb") as fh:
        fh.write(png_bytes(MASTER, MASTER, master_rgba))

    # Window icon: same supersampled sums, 16x box filter (4 master px).
    win_acc = downsample_sums(acc, MASTER, MASTER // WINDOW)
    win_rgba = finalize(win_acc, n * (MASTER // WINDOW) ** 2, WINDOW)
    rgba_path = os.path.join(assets, "icon-256.rgba")
    with open(rgba_path, "wb") as fh:
        fh.write(win_rgba)

    print("wrote %s (%d bytes)" % (png_path, os.path.getsize(png_path)))
    print("wrote %s (%d bytes)" % (rgba_path, os.path.getsize(rgba_path)))

    icns_path = os.path.join(assets, "icon.icns")
    if build_icns(png_path, icns_path):
        print("wrote %s (%d bytes)" % (icns_path, os.path.getsize(icns_path)))


if __name__ == "__main__":
    sys.exit(main())
