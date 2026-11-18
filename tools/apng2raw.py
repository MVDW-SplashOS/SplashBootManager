#!/usr/bin/env python3
"""Convert an APNG boot animation to the SPLASHLOGO2 framebuffer asset.

The framebuffer player (../splash-logo.c) is deliberately dependency-free:
it cannot decode PNG, so the animation is converted at packaging time to a
flat, memory-mappable frame array:

    offset 0   char[12]  "SPLASHLOGO2\\0"
    offset 12  u32 LE    width
    offset 16  u32 LE    height
    offset 20  u32 LE    frame count
    offset 24  u32 LE    frame delay in milliseconds
    offset 28  nframes * width * height * 3 bytes, BGR (little-endian
               0xBBGGRR), frames composited over the black background

24-bit BGR keeps the smooth gradients of the source: the GIF palette (256
colours) and half-resolution raw of the first asset could not.  Storing the
asset xz-compressed in git keeps the repository small; the build script
decompresses it when installing.

Only zlib is needed (PNG/DEFLATE); no third-party imaging library.

Usage:
    tools/apng2raw.py splashos_boot.apng -o assets/boot-logo.raw
    tools/apng2raw.py splashos_boot.apng -o out.raw --height 400
"""

import argparse
import struct
import sys
import zlib

MAGIC = b"SPLASHLOGO2\x00"


# ---------------------------------------------------------------------------
# APNG decoding
# ---------------------------------------------------------------------------

def _chunks(data):
    off = 8
    while off < len(data):
        length = struct.unpack(">I", data[off:off + 4])[0]
        yield data[off + 4:off + 8], data[off + 8:off + 8 + length]
        off += 12 + length


def _paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    return b if pb <= pc else c


def _unfilter(raw, width, height, bpp):
    stride = width * bpp
    out = bytearray(stride * height)
    prev = bytearray(stride)
    pos = 0
    for y in range(height):
        kind = raw[pos]
        pos += 1
        line = bytearray(raw[pos:pos + stride])
        pos += stride
        if kind == 1:
            for i in range(bpp, stride):
                line[i] = (line[i] + line[i - bpp]) & 0xFF
        elif kind == 2:
            for i in range(stride):
                line[i] = (line[i] + prev[i]) & 0xFF
        elif kind == 3:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + ((left + prev[i]) >> 1)) & 0xFF
        elif kind == 4:
            for i in range(stride):
                left = line[i - bpp] if i >= bpp else 0
                upleft = prev[i - bpp] if i >= bpp else 0
                line[i] = (line[i] + _paeth(left, prev[i], upleft)) & 0xFF
        out[y * stride:(y + 1) * stride] = line
        prev = line
    return out


def decode_apng(path):
    """Return (width, height, [(rgba_bytes, delay_ms), ...])."""
    data = open(path, "rb").read()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise SystemExit(f"{path}: not a PNG file")

    width = height = 0
    canvas = None
    pending = None
    payload = bytearray()
    frames = []

    for kind, body in _chunks(data):
        if kind == b"IHDR":
            width, height, depth, ctype, _, _, _ = struct.unpack(">IIBBBBB", body)
            if depth != 8 or ctype != 6:
                raise SystemExit(
                    f"{path}: expected 8-bit RGBA (got depth {depth}, type {ctype})")
            canvas = bytearray(width * height * 4)
        elif kind == b"fcTL":
            if pending is not None:
                _compose(canvas, width, pending, payload, frames)
            pending = struct.unpack(">IIIIIHHBB", body)[1:]
            payload = bytearray()
        elif kind == b"IDAT":
            payload += body
        elif kind == b"fdAT":
            payload += body[4:]
        elif kind == b"IEND" and pending is not None:
            _compose(canvas, width, pending, payload, frames)
            pending = None

    if not frames:
        raise SystemExit(f"{path}: no animation frames (plain PNG?)")
    return width, height, frames


def _compose(canvas, width, rect, payload, frames):
    fw, fh, fx, fy, delay_num, delay_den, dispose, blend = rect
    pixels = _unfilter(zlib.decompress(payload), fw, fh, 4)
    before = bytes(canvas)
    stride = width * 4

    for y in range(fh):
        src = pixels[y * fw * 4:(y + 1) * fw * 4]
        base = (fy + y) * stride + fx * 4
        if blend == 0:                                  # APNG_BLEND_OP_SOURCE
            canvas[base:base + fw * 4] = src
            continue
        for x in range(fw):                             # APNG_BLEND_OP_OVER
            s = x * 4
            d = base + s
            alpha = src[s + 3]
            if alpha == 255:
                canvas[d:d + 4] = src[s:s + 4]
            elif alpha:
                inv = 255 - alpha
                for k in range(3):
                    canvas[d + k] = (src[s + k] * alpha
                                     + canvas[d + k] * inv + 127) // 255
                canvas[d + 3] = alpha + (canvas[d + 3] * inv + 127) // 255

    delay = round(1000 * delay_num / delay_den) if delay_den else 0
    frames.append((bytes(canvas), delay))

    if dispose == 1:                                    # APNG_DISPOSE_OP_BACKGROUND
        for y in range(fh):
            off = (fy + y) * stride + fx * 4
            canvas[off:off + fw * 4] = b"\x00" * (fw * 4)
    elif dispose == 2:                                  # APNG_DISPOSE_OP_PREVIOUS
        canvas[:] = before


# ---------------------------------------------------------------------------
# Resampling + asset writing
# ---------------------------------------------------------------------------

def _spans(src, dst):
    """Destination -> source coverage, same kernel as splash-logo.c.

    Each entry is a list of (source_index, weight) with weights summing to
    one (in 1/65536 units).  Box coverage over the destination pixel gives
    area averaging when shrinking and linear interpolation when growing.
    """
    table = []
    for d in range(dst):
        lo = d * src * 65536 // dst
        hi = (d + 1) * src * 65536 // dst
        entry = []
        for i in range(lo >> 16, min(((hi + 65535) >> 16), src)):
            s = i << 16
            entry.append((i, min(hi, s + 65536) - max(lo, s)))
        table.append(entry)
    return table


def resize_to_bgr(rgba, sw, sh, tw, th):
    """Alpha-composite over black and box-filter to tw x th, BGR output."""
    xs = _spans(sw, tw)
    ys = _spans(sh, th)
    out = bytearray(tw * th * 3)
    for y in range(th):
        drow = y * tw * 3
        for x in range(tw):
            b = g = r = total = 0
            for sy, wy in ys[y]:
                srow = sy * sw * 4
                for sx, wx in xs[x]:
                    s = srow + sx * 4
                    w = wy * wx
                    alpha = rgba[s + 3]
                    # premultiplied over black, weighted
                    b += w * rgba[s + 2] * alpha
                    g += w * rgba[s + 1] * alpha
                    r += w * rgba[s] * alpha
                    total += w
            d = drow + x * 3
            if total:
                out[d + 0] = (b + total * 127) // (total * 255)
                out[d + 1] = (g + total * 127) // (total * 255)
                out[d + 2] = (r + total * 127) // (total * 255)
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("input", help="source .apng")
    ap.add_argument("-o", "--output", default="boot-logo.raw",
                    help="output SPLASHLOGO2 asset (default: boot-logo.raw)")
    ap.add_argument("--height", type=int, default=0,
                    help="output height in pixels (default: native)")
    ap.add_argument("--delay-ms", type=int, default=0,
                    help="frame delay in ms (default: from the APNG)")
    args = ap.parse_args()

    sw, sh, frames = decode_apng(args.input)
    th = args.height or sh
    tw = max(1, round(sw * th / sh))
    delay = args.delay_ms or frames[0][1]

    print(f"{args.input}: {sw}x{sh}, {len(frames)} frames, "
          f"{delay} ms/frame -> {tw}x{th} BGR24")

    with open(args.output, "wb") as out:
        out.write(MAGIC + struct.pack("<IIII", tw, th, len(frames), delay))
        for i, (rgba, _) in enumerate(frames):
            out.write(resize_to_bgr(rgba, sw, sh, tw, th))
            if (i + 1) % 10 == 0:
                print(f"  {i + 1}/{len(frames)}", file=sys.stderr)
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
