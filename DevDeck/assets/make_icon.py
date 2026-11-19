"""Renders the DevDeck app icon (assets/devdeck.ico and a 256 px PNG preview).

One bold mark on a blue-violet tile: a terminal prompt (">_") whose chevron is a
Git branch, with commit nodes at its two ends, and a green cursor for the
underscore. Every size is drawn on its own (supersampled, then
downscaled) so the small sizes stay crisp, and the smallest sizes use thicker
strokes and solid nodes.

    python assets/make_icon.py
"""

import os
import struct
from io import BytesIO

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SS = 8  # supersampling factor

TOP = (37, 99, 235)  # blue-600
BOTTOM = (109, 40, 217)  # violet-700
MARK = (255, 255, 255)
CURSOR = (52, 211, 153)  # emerald-400
SHADOW = (30, 27, 75)


def lerp(a, b, t):
    return tuple(int(round(a[i] + (b[i] - a[i]) * t)) for i in range(len(a)))


def tile(n):
    """Rounded square with a diagonal gradient."""
    grad = Image.new("RGBA", (n, n))
    px = grad.load()
    for y in range(n):
        for x in range(n):
            t = (x * 0.35 + y * 0.65) / n
            px[x, y] = lerp(TOP, BOTTOM, min(1.0, t)) + (255,)
    mask = Image.new("L", (n, n), 0)
    ImageDraw.Draw(mask).rounded_rectangle((0, 0, n - 1, n - 1), radius=int(n * 0.22), fill=255)
    out = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    out.paste(grad, (0, 0), mask)
    return out


def mark(n, size):
    """The ">_" mark on a transparent layer."""
    u = n / 100.0
    small = size <= 24
    lay = Image.new("RGBA", (n, n), (0, 0, 0, 0))
    d = ImageDraw.Draw(lay)
    w = (13 if small else 10) * u  # stroke width
    nr = (10 if small else 9) * u  # node radius
    a, b, c = (27 * u, 26 * u), (60 * u, 50 * u), (27 * u, 74 * u)
    d.line((a, b, c), fill=MARK + (255,), width=int(w), joint="curve")
    # round tip of the chevron
    h = w / 2
    d.ellipse((b[0] - h, b[1] - h, b[0] + h, b[1] + h), fill=MARK + (255,))
    for x, y in (a, c):
        d.ellipse((x - nr, y - nr, x + nr, y + nr), fill=MARK + (255,))
    if not small:
        # hollow commit nodes: punch the centers out of the layer
        hole = nr * 0.45
        for x, y in (a, c):
            d.ellipse((x - hole, y - hole, x + hole, y + hole), fill=(0, 0, 0, 0))
    # the cursor: a rounded bar under the prompt
    y = c[1]
    d.rounded_rectangle((58 * u, y - h, 80 * u, y + h), radius=h, fill=CURSOR + (255,))
    return lay


def render(size):
    n = size * SS
    img = tile(n)
    m = mark(n, size)
    # soft shadow: the mark's alpha, tinted and nudged down
    if size >= 32:
        alpha = m.split()[3]
        sh = Image.new("RGBA", (n, n), SHADOW + (0,))
        sh.putalpha(alpha.point(lambda v: v * 70 // 255))
        img = Image.alpha_composite(img, sh.transform((n, n), Image.AFFINE, (1, 0, 0, 0, 1, -n * 0.02)))
    img = Image.alpha_composite(img, m)
    return img.resize((size, size), Image.LANCZOS)


def write_ico(path, images):
    """ICO with one PNG-compressed entry per size (supported since Windows Vista)."""
    blobs = []
    for im in images:
        b = BytesIO()
        im.save(b, "PNG", optimize=True)
        blobs.append(b.getvalue())
    out = BytesIO()
    out.write(struct.pack("<HHH", 0, 1, len(images)))
    offset = 6 + 16 * len(images)
    for im, data in zip(images, blobs):
        s = im.size[0]
        out.write(struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(data), offset))
        offset += len(data)
    for data in blobs:
        out.write(data)
    with open(path, "wb") as f:
        f.write(out.getvalue())


def main():
    images = [render(s) for s in SIZES]
    write_ico(os.path.join(HERE, "devdeck.ico"), images)
    images[-1].save(os.path.join(HERE, "devdeck.png"))
    # preview sheet of every size on light and dark backgrounds
    w = sum(SIZES) + 10 * (len(SIZES) + 1)
    sheet = Image.new("RGBA", (w, 2 * 276), (255, 255, 255, 255))
    ImageDraw.Draw(sheet).rectangle((0, 276, w, 2 * 276), fill=(30, 30, 30, 255))
    x = 10
    for s, im in zip(SIZES, images):
        sheet.paste(im, (x, 10), im)
        sheet.paste(im, (x, 286), im)
        x += s + 10
    sheet.save(os.path.join(HERE, "preview.png"))
    print("ok")


if __name__ == "__main__":
    main()
