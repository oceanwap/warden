#!/usr/bin/env python3
"""Subset the fonts the GUI embeds, and write gui/src/icons.rs.

    pip install fonttools brotli && python3 gui/assets/fonts/build.py [--cache DIR]

Sources (downloaded once into --cache, default a temp directory):
  Inter 4.1            https://github.com/rsms/inter                 SIL OFL 1.1
  JetBrains Mono 2.304 https://github.com/JetBrains/JetBrainsMono    SIL OFL 1.1
  Lucide (lucide-static 1.49.0 from npm, the icon font)               ISC

The outputs are committed (building Warden needs no Python). Subsetting keeps
the binary small: the text fonts to Latin, punctuation, arrows and box
drawing (any other script falls back to the system's fonts), the icon font to
the icons named in ICONS below.
"""
import io
import json
import sys
import tarfile
import tempfile
import urllib.request
import zipfile
from pathlib import Path

from fontTools import subset

here = Path(__file__).resolve().parent
root = here.parent.parent  # gui/
cache = Path(sys.argv[sys.argv.index("--cache") + 1]) if "--cache" in sys.argv else Path(tempfile.gettempdir()) / "warden-fonts"
cache.mkdir(parents=True, exist_ok=True)

INTER = "https://github.com/rsms/inter/releases/download/v4.1/Inter-4.1.zip"
JETBRAINS = "https://github.com/JetBrains/JetBrainsMono/releases/download/v2.304/JetBrainsMono-2.304.zip"
LUCIDE = "https://registry.npmjs.org/lucide-static/-/lucide-static-1.49.0.tgz"

# (Rust name, Lucide name). The Rust name is what the GUI's code says.
ICONS = [
    ("ChevronDown", "chevron-down"), ("ChevronRight", "chevron-right"), ("ChevronUp", "chevron-up"),
    ("Restart", "rotate-cw"), ("Reset", "rotate-ccw"), ("Reload", "refresh-cw"), ("Rolling", "layers"),
    ("SafeReload", "shield-check"), ("Hard", "zap"), ("Play", "play"), ("Stop", "square"), ("Power", "power"),
    ("Plus", "plus"), ("Minus", "minus"), ("Settings", "settings"), ("Edit", "file-pen"), ("Copy", "copy"),
    ("Check", "check"), ("Close", "x"), ("Search", "search"), ("Server", "server"), ("Globe", "globe"),
    ("Plug", "plug"), ("Unplug", "unplug"), ("Laptop", "laptop"), ("Activity", "activity"),
    ("Logs", "scroll-text"), ("History", "chart-line"), ("Cpu", "cpu"), ("Memory", "memory-stick"),
    ("Clock", "clock"), ("Alert", "triangle-alert"), ("AlertCircle", "circle-alert"),
    ("CheckCircle", "circle-check"), ("Shield", "shield"), ("Terminal", "terminal"), ("Link", "link"),
    ("ExternalLink", "external-link"), ("User", "user"), ("Network", "network"), ("Ellipsis", "ellipsis"),
    ("Pause", "pause"), ("Info", "info"), ("Loader", "loader"), ("Wifi", "wifi"), ("WifiOff", "wifi-off"),
    ("Boxes", "boxes"), ("Workers", "layers"), ("Folder", "folder"), ("Timer", "timer"),
]

TEXT_UNICODES = (
    list(range(0x20, 0x7F)) + list(range(0xA0, 0x180)) + list(range(0x2010, 0x2030)) + [0x2039, 0x203A]
    + list(range(0x2190, 0x21A0)) + [0x2212, 0x00D7, 0x2713, 0x2715, 0x25CF, 0x25B2, 0x25BC, 0x25B6, 0x2022]
)
MONO_UNICODES = TEXT_UNICODES + list(range(0x2500, 0x25A0))


def fetch(url: str) -> bytes:
    f = cache / url.rsplit("/", 1)[1]
    if not f.exists():
        print("downloading", url)
        req = urllib.request.Request(url, headers={"User-Agent": "warden-build"})
        f.write_bytes(urllib.request.urlopen(req).read())
    return f.read_bytes()


def write_subset(data: bytes, unicodes, out: Path) -> None:
    opts = subset.Options()
    opts.layout_features = ["kern", "liga", "calt", "ccmp", "locl", "mark", "mkmk", "case", "tnum"]
    opts.name_IDs = ["*"]
    opts.hinting = False
    opts.notdef_outline = True
    opts.drop_tables += ["DSIG"]
    font = subset.load_font(io.BytesIO(data), opts)
    s = subset.Subsetter(opts)
    s.populate(unicodes=unicodes)
    s.subset(font)
    subset.save_font(font, str(out), opts)
    print(f"{out.name}: {out.stat().st_size // 1024} KB")


inter = zipfile.ZipFile(io.BytesIO(fetch(INTER)))
for weight in ("Regular", "Medium", "SemiBold"):
    write_subset(inter.read(f"extras/ttf/Inter-{weight}.ttf"), TEXT_UNICODES, here / f"Inter-{weight}.ttf")
jb = zipfile.ZipFile(io.BytesIO(fetch(JETBRAINS)))
write_subset(jb.read("fonts/ttf/JetBrainsMono-Regular.ttf"), MONO_UNICODES, here / "JetBrainsMono-Regular.ttf")

lucide = tarfile.open(fileobj=io.BytesIO(fetch(LUCIDE)))
codepoints = json.load(lucide.extractfile("package/font/codepoints.json"))
wanted = sorted({codepoints[name] for _, name in ICONS})
write_subset(lucide.extractfile("package/font/lucide.ttf").read(), wanted, here / "lucide.ttf")

licenses = [
    "Fonts embedded in warden-gui\n============================\n",
    "Inter (c) The Inter Project Authors, SIL Open Font License 1.1 (https://github.com/rsms/inter)\n"
    "JetBrains Mono (c) JetBrains s.r.o., SIL Open Font License 1.1 (https://github.com/JetBrains/JetBrainsMono)\n"
    "Lucide icons (c) Lucide Contributors, ISC License; parts (c) Cole Bemis 2013-2022, MIT License (https://lucide.dev)\n",
    "\n---- Inter: LICENSE.txt ----\n" + inter.read("LICENSE.txt").decode(),
    "\n---- JetBrains Mono: OFL.txt ----\n" + jb.read("OFL.txt").decode(),
    "\n---- Lucide: LICENSE ----\n" + lucide.extractfile("package/LICENSE").read().decode(),
]
(here / "LICENSES.txt").write_text("\n".join(licenses))

# gui/src/icons.rs
lines = [
    "//! The icons of the GUI: Lucide glyphs (ISC) in the font gui/assets/fonts/lucide.ttf.",
    "//! Generated by gui/assets/fonts/build.py: change the list there.",
    "",
    "/// An icon of the embedded font; `.glyph()` is its character.",
    "#[derive(Debug, Clone, Copy, PartialEq, Eq)]",
    "pub enum Icon {",
]
seen, arms = set(), []
for rust, name in ICONS:
    lines.append(f"    {rust},")
    arms.append(f"            Icon::{rust} => '\\u{{{codepoints[name]:x}}}', // {name}")
lines += [
    "}",
    "",
    "impl Icon {",
    "    pub const fn glyph(self) -> char {",
    "        match self {",
    *arms,
    "        }",
    "    }",
    "}",
    "",
]
(root / "src" / "icons.rs").write_text("\n".join(lines))
print("icons.rs:", len(ICONS), "icons")
