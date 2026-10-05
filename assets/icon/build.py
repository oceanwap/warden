#!/usr/bin/env python3
"""Render the Warden icon from warden.svg into every format the project uses.

    pip install cairosvg pillow && python3 assets/icon/build.py

Writes (all committed, so building Warden needs no Python):
  assets/icon/png/warden-<size>.png   the full icon with its margin (README, Linux packages)
  assets/icon/Warden.icns             macOS (Warden.app, Contents/Resources): full bleed, see below
  gui/assets/icon-128.rgba            the window icon of warden-gui (128 x 128, raw RGBA)
"""
import io
import struct
from pathlib import Path

import cairosvg
from PIL import Image

here = Path(__file__).resolve().parent
root = here.parent.parent
svg = here / "warden.svg"


def render(size: int) -> Image.Image:
    png = cairosvg.svg2png(url=str(svg), output_width=size, output_height=size)
    return Image.open(io.BytesIO(png)).convert("RGBA")


def png_bytes(img: Image.Image) -> bytes:
    out = io.BytesIO()
    img.save(out, "PNG", optimize=True)
    return out.getvalue()


big = render(2048)


def at(size: int) -> Image.Image:
    return big.resize((size, size), Image.LANCZOS)


# PNGs.
(here / "png").mkdir(exist_ok=True)
for size in (16, 32, 64, 128, 256, 512, 1024):
    (here / "png" / f"warden-{size}.png").write_bytes(png_bytes(at(size)))

# macOS .icns: full bleed. Since macOS 26 the system draws the rounded square itself, and an icon
# that leaves a margin (the older template, as in warden.svg) is shrunk onto a gray plate. So the
# plate's gradient fills the whole canvas, square, without the shadow, and the shield and W are
# scaled up with it (1024 / 824), keeping their size on the plate.
def full_bleed_svg() -> str:
    text = svg.read_text()
    start = text.index("  <!-- the macOS icon plate")
    end = text.index("  <!-- shield -->")
    plate = (
        '  <rect width="1024" height="1024" fill="url(#plate)"/>\n'
        '  <rect width="1024" height="1024" fill="url(#glow)"/>\n'
        '  <g transform="translate(512 512) scale(1.2427) translate(-512 -512)">\n'
    )
    text = text[:start] + plate + text[end:]
    return text.replace("</svg>", "  </g>\n</svg>")


bleed = Image.open(
    io.BytesIO(cairosvg.svg2png(bytestring=full_bleed_svg().encode(), output_width=2048, output_height=2048))
).convert("RGBA")


def bleed_at(size: int) -> Image.Image:
    return bleed.resize((size, size), Image.LANCZOS)


# PNG-compressed entries (macOS 10.7 and later).
entries = [
    (b"icp4", 16), (b"icp5", 32), (b"icp6", 64),
    (b"ic07", 128), (b"ic08", 256), (b"ic09", 512), (b"ic10", 1024),
    (b"ic11", 32), (b"ic12", 64), (b"ic13", 256), (b"ic14", 512),
]
chunks = b"".join(kind + struct.pack(">I", 8 + len(data)) + data for kind, data in ((k, png_bytes(bleed_at(s))) for k, s in entries))
(here / "Warden.icns").write_bytes(b"icns" + struct.pack(">I", 8 + len(chunks)) + chunks)

# The window icon: the plate fills the image (the margin and shadow of the
# macOS template are not wanted on a window or a taskbar).
plate = big.crop((200, 200, 1848, 1848)).resize((128, 128), Image.LANCZOS)
(root / "gui" / "assets").mkdir(parents=True, exist_ok=True)
(root / "gui" / "assets" / "icon-128.rgba").write_bytes(plate.tobytes())
print("icon files written")
