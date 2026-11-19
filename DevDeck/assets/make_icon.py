"""Renders the DevDeck app icon (assets/devdeck.ico and a 256 px PNG preview).

A fanned deck of cards on a blue-violet tile; the front card carries a Git branch
graph. Every size is drawn on its own (supersampled, then downscaled) so the small
sizes stay crisp, and the smallest sizes use a simplified drawing.

    python assets/make_icon.py
"""

import os
import struct
from io import BytesIO

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 128, 256]
SS = 8  # supersampling factor

TOP = (59, 130, 246)  # blue
BOTTOM = (109, 40, 217)  # violet
CARD = (248, 250, 252)
INK = (67, 56, 202)  # indigo, for the branch graph
ACCENT = (16, 185, 129)  # green branch head


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


def layer(img, draw_fn):
    """Draws on a transparent layer and alpha-blends it onto `img`."""
    lay = Image.new("RGBA", img.size, (0, 0, 0, 0))
    draw_fn(ImageDraw.Draw(lay))
    return Image.alpha_composite(img, lay)


def render(size):
    n = size * SS
    small = size <= 24
    img = tile(n)
    u = n / 100.0  # drawing unit: 1/100 of the icon

    # the deck: the cards behind the front one are fanned out to the upper right
    fl, ft, fr, fb = 15 * u, 35 * u, 73 * u, 87 * u
    r = 7 * u
    backs = [(10, 0.45)] if small else [(16, 0.30), (8, 0.55)]
    for off, alpha in backs:
        o = off * u
        img = layer(img, lambda d: d.rounded_rectangle((fl + o, ft - o, fr + o, fb - o), radius=r, fill=CARD + (int(255 * alpha),)))
    # soft shadow under the front card, then the card
    img = layer(img, lambda d: d.rounded_rectangle((fl + 1.5 * u, ft + 2 * u, fr + 1.5 * u, fb + 2 * u), radius=r, fill=(30, 27, 75, 80)))
    d = ImageDraw.Draw(img)
    d.rounded_rectangle((fl, ft, fr, fb), radius=r, fill=CARD + (255,))

    # branch graph on the front card: a main line with a branch curving off it
    w = (8.0 if small else 5.5) * u
    nr = (8.0 if small else 6.0) * u
    cx = (fl + fr) / 2
    x0, x1 = cx - 11 * u, cx + 11 * u
    y_top, y_mid, y_bot = ft + 13 * u, ft + 26 * u, fb - 12 * u
    d.line((x0, y_top, x0, y_bot), fill=INK + (255,), width=int(w))
    steps = 32
    pts = []
    sx, sy = x0, y_bot - 8 * u
    ex, ey = x1, y_top + 2 * u
    c1 = (x0, y_mid)
    c2 = (x1, y_mid + 4 * u)
    for i in range(steps + 1):
        t = i / steps
        bx = (1 - t) ** 3 * sx + 3 * (1 - t) ** 2 * t * c1[0] + 3 * (1 - t) * t * t * c2[0] + t ** 3 * ex
        by = (1 - t) ** 3 * sy + 3 * (1 - t) ** 2 * t * c1[1] + 3 * (1 - t) * t * t * c2[1] + t ** 3 * ey
        pts.append((bx, by))
    d.line(pts, fill=INK + (255,), width=int(w), joint="curve")
    for px, py, col in [(x0, y_top, INK), (x0, y_bot, INK), (x1, y_top, ACCENT)]:
        d.ellipse((px - nr, py - nr, px + nr, py + nr), fill=col + (255,))
        if not small:
            hole = nr * 0.45
            d.ellipse((px - hole, py - hole, px + hole, py + hole), fill=CARD + (255,))

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
