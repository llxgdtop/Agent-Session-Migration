#!/usr/bin/env python3
"""Generate the Agent Session Hub icon assets (pure Python 3 stdlib).

Outputs (into <repo>/assets, created if missing):
  icon-1024.png  master icon, RGBA PNG with straight alpha
  icon-256.rgba  256x256 raw RGBA bytes (straight alpha), embedded into the
                 eframe/egui window icon via include_bytes!
  icon.icns      macOS icon resource (only when sips + iconutil are
                 available; skipped silently elsewhere)

Design: a rounded-square background (corner radius 22.5% of the side)
with a vertical gradient from deep indigo #3B5BDB (top) to deep
blue-purple #2B3A8F (bottom), and a centered white glyph: two opposing
arc arrows chasing each other around a ring (the session-interchange
metaphor). Each arc spans 120 degrees; the two 60-degree gaps sit on the
left/right horizontal axis, and an arrowhead triangle at each leading
end points along the direction of travel.

Rendering: analytic coverage rasterization on a 4x supersampled canvas
(4096x4096 for the 1024 master) -- every sample is classified
exactly (inside/outside per shape, no approximation) -- followed by a
box filter down to the output size. The 256 icon is derived from the
same supersampled sums with a 16x box filter. Output uses straight
(non-premultiplied) alpha, which is what both PNG and egui::IconData
expect.

Determinism: no randomness and no timestamps; all arithmetic runs in a
fixed evaluation order, so repeated runs on the same interpreter
produce byte-identical files.

Usage: python3 scripts/gen_icon.py   (works from any cwd)
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

# ---------------------------------------------------------------------------
# Design constants (unit-square coordinates: x right, y down, side = 1.0)
# ---------------------------------------------------------------------------

MASTER = 1024            # master PNG size
WINDOW = 256             # egui window-icon size
SS = 4                   # supersampling factor per axis (4096 canvas)

GRAD_TOP = (0x3B, 0x5B, 0xDB)  # deep indigo, top of the gradient
GRAD_BOT = (0x2B, 0x3A, 0x8F)  # deep blue-purple, bottom of the gradient

CORNER = 0.225           # rounded-square corner radius (fraction of side)
RING_R = 0.245           # ring mid radius (center line of the stroke)
RING_HALF = 0.040        # ring stroke half width (stroke = 8% of side)
ARC_SPAN = math.radians(60.0)  # half span of each arc (full span 120 deg)
ARC_CENTERS = (math.radians(270.0), math.radians(90.0))  # top, bottom arc
HEAD_LEN = 0.115         # arrowhead length along the travel direction
HEAD_HALF = 0.060        # arrowhead half width (1.5x stroke half width)


# ---------------------------------------------------------------------------
# Glyph geometry
# ---------------------------------------------------------------------------

def build_glyph(scale):
    """Precompute glyph rasterization primitives at `scale` subpixels.

    Returns (r1sq, r2sq, arcs, heads):
      r1sq/r2sq -- squared inner/outer ring radii
      arcs      -- per arc two wedge half-planes (nx, ny) keeping samples
                   with nx*dx + ny*dy <= 0 (exact for spans < 180 deg)
      heads     -- per arrowhead three edge functions (A, B, C) that are
                   >= 0 inside the triangle, plus a bounding box
    """
    r1 = (RING_R - RING_HALF) * scale
    r2 = (RING_R + RING_HALF) * scale
    arcs = []
    heads = []
    for tc in ARC_CENTERS:
        te = tc + ARC_SPAN  # leading end (arrowhead sits here)
        planes = tuple(
            (math.cos(a), math.sin(a))
            for a in (te + math.pi / 2.0, tc - ARC_SPAN - math.pi / 2.0)
        )
        arcs.append(planes)
        # Arrowhead: base centered on the arc end, tip along the tangent
        # (direction of travel), width across the tangent.
        cx = cy = 0.5 * scale
        px = cx + (RING_R * scale) * math.cos(te)
        py = cy + (RING_R * scale) * math.sin(te)
        tx, ty = -math.sin(te), math.cos(te)
        nx, ny = -ty, tx
        verts = (
            (px + tx * HEAD_LEN * scale, py + ty * HEAD_LEN * scale),
            (px + nx * HEAD_HALF * scale, py + ny * HEAD_HALF * scale),
            (px - nx * HEAD_HALF * scale, py - ny * HEAD_HALF * scale),
        )
        edges = []
        gx = sum(v[0] for v in verts) / 3.0
        gy = sum(v[1] for v in verts) / 3.0
        for k in range(3):
            x0, y0 = verts[k]
            x1, y1 = verts[(k + 1) % 3]
            a, b, c = y0 - y1, x1 - x0, x0 * y1 - x1 * y0
            if a * gx + b * gy + c < 0.0:
                a, b, c = -a, -b, -c
            edges.append((a, b, c))
        box = (
            min(v[0] for v in verts) - 1.0,
            max(v[0] for v in verts) + 1.0,
            min(v[1] for v in verts) - 1.0,
            max(v[1] for v in verts) + 1.0,
        )
        heads.append((edges, box))
    return r1 * r1, r2 * r2, arcs, heads


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
    half = 0.5 * canvas
    corner = CORNER * canvas
    inner = half - corner  # inner (straight-edge) rect half extent
    r1sq, r2sq, arcs, heads = build_glyph(canvas)
    ring_outer = math.sqrt(r2sq)

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

        # --- Pass 2: glyph inside its row envelope ---------------------
        dy2 = dy * dy
        gxl = gxr = None
        if dy2 <= r2sq:
            xr = math.sqrt(r2sq - dy2)
            gxl, gxr = -xr, xr
        active_heads = []
        for head in heads:
            box = head[1]
            if box[2] <= y <= box[3]:
                active_heads.append(head)
                if gxl is None or box[0] - c < gxl:
                    gxl = box[0] - c
                if gxr is None or box[1] - c > gxr:
                    gxr = box[1] - c
        if gxl is None:
            continue
        # Per-row wedge constants: nx*dx <= -ny*dy
        row_arcs = [
            ((p[0][0], p[1][0]), (-p[0][1] * dy, -p[1][1] * dy)) for p in arcs
        ]
        row_edges = [
            (e[0][0], e[1][0], e[2][0],
             e[0][1] * y + e[0][2], e[1][1] * y + e[1][2], e[2][1] * y + e[2][2])
            for (e, _box) in active_heads
        ]
        glo = max(0, int(math.ceil(c + gxl - 0.5)))
        ghi = min(canvas - 1, int(math.floor(c + gxr - 0.5)))
        for sx in range(glo, ghi + 1):
            x = sx + 0.5
            dx = x - c
            hit = False
            d2 = dx * dx + dy2
            if r1sq <= d2 <= r2sq:
                for (nx1, nx2), (k1, k2) in row_arcs:
                    if nx1 * dx <= k1 and nx2 * dx <= k2:
                        hit = True
                        break
            if not hit:
                for e in row_edges:
                    if (
                        e[0] * x + e[3] >= 0.0
                        and e[1] * x + e[4] >= 0.0
                        and e[2] * x + e[5] >= 0.0
                    ):
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
