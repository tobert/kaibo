#!/usr/bin/env python3
"""Generate the kaibo brand spread from Kaile, the mascot.

Inputs (both in-repo, no network):
  ../kaile.svg            the original line-art (4 compound paths), untouched
  kaile-silhouette.svg    a potrace vectorization of the body fill, used by the
                          filled-green variants (see "How the silhouette was made")

Outputs (this directory): logo-*.svg and banner-*.svg in a few color directions.
Re-run after editing to regenerate:  python3 generate.py
Render a preview:  rsvg-convert banner-teal.svg -o /tmp/x.png

How the silhouette was made (reproduce if kaile.svg changes):
  rsvg-convert -w 1321 ../kaile.svg -b white -o /tmp/onwhite.png
  magick /tmp/onwhite.png -fuzz 30% -fill '#f0f' -draw 'color 0,0 floodfill' \
         -fuzz 25% -fill black -opaque white -fuzz 25% -fill white -opaque '#f0f' /tmp/sil.png
  magick /tmp/sil.png -threshold 50% /tmp/sil.pbm
  potrace -s /tmp/sil.pbm -o kaile-silhouette.svg
The 1321px render is exactly 2x the native viewBox, so SCL below maps it back.

Fonts: Latin from the run, kanji 解剖 from Noto Sans CJK JP (pacman: noto-fonts-cjk).
"""
import re, pathlib

HERE = pathlib.Path(__file__).resolve().parent

# --- source geometry -------------------------------------------------------
LINE = re.findall(r'<path d="(.*?)"', (HERE / "../kaile.svg").read_text(), re.S)
SIL  = re.findall(r'<path d="(.*?)"', (HERE / "kaile-silhouette.svg").read_text(), re.S)[0]
VB    = "0 0 660.42 629.23"                 # native creature box
SCL   = 660.42 / 1321.0                     # potrace bitmap (2x) -> native units
SIL_T = f"scale({SCL}) translate(0,1259) scale(0.1,-0.1)"
FONT  = "Noto Sans CJK JP"

def lineart(fill):
    return f'<g fill="{fill}">' + "".join(f'<path d="{d}"/>' for d in LINE) + "</g>"

def silhouette(fill):
    return f'<g transform="{SIL_T}" fill="{fill}"><path d="{SIL}"/></g>'

def kaile(mode, body=None, ink="#000"):
    """'line' = ink outline only; 'fill' = body silhouette under the ink outline."""
    return (silhouette(body) + lineart(ink)) if mode == "fill" else lineart(ink)

def place(inner, x, y, w, h, par="xMidYMid meet"):
    return (f'<svg x="{x}" y="{y}" width="{w}" height="{h}" '
            f'viewBox="{VB}" preserveAspectRatio="{par}">{inner}</svg>')

def wordmark(x, y, color, size=104):
    return (f'<text x="{x}" y="{y}" font-family="{FONT}" font-weight="700" '
            f'font-size="{size}" fill="{color}" letter-spacing="-2" '
            f'dominant-baseline="central">kaibo 解剖</text>')

def svg(w, h, body):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
            f'viewBox="0 0 {w} {h}">{body}</svg>\n')

def save(name, content):
    (HERE / name).write_text(content)
    print("wrote", name)

# ============================ LOGOS (512) =================================
R = 96  # badge corner radius
save("logo-teal.svg", svg(512, 512,
    f'<rect width="512" height="512" rx="{R}" fill="#f3ead4"/>'
    + place(kaile("line", ink="#0d4d4d"), 36, 36, 440, 440)))
save("logo-green.svg", svg(512, 512,
    f'<rect width="512" height="512" rx="{R}" fill="#1b1e22"/>'
    + place(kaile("fill", body="#55ad52", ink="#0c0d0f"), 36, 36, 440, 440)))
save("logo-mint.svg", svg(512, 512,
    '<circle cx="256" cy="256" r="248" fill="#daefe3"/>'
    + place(kaile("line", ink="#1f2a28"), 56, 56, 400, 400)))

# ============================ BANNERS (1280x640) =========================
def banner(name, bg, body, ink, wm, fill=False):
    art = kaile("fill", body=body, ink=ink) if fill else kaile("line", ink=ink)
    save(name, svg(1280, 640,
        f'<rect width="1280" height="640" fill="{bg}"/>'
        + place(art, 60, 50, 540, 540)
        + wordmark(636, 320, wm)))

banner("banner-teal.svg",   "#f3ead4", None,      "#0d4d4d", "#0d4d4d")
banner("banner-green.svg",  "#1b1e22", "#55ad52", "#0c0d0f", "#f3ead4", fill=True)
banner("banner-invert.svg", "#0d4d4d", None,      "#f3ead4", "#f3ead4")
print("done")
