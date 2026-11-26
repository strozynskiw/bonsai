#!/usr/bin/env python3
"""Render a captured ANSI screen into a PNG for the docs (stdlib only).

The TUI screenshots in the README are real terminal output, not mock-ups: the
frame is captured from a live session with ANSI colours intact, then painted
here with one absolutely positioned glyph per terminal cell so the layout
survives any font substitution.

Capture a frame from a running session:

    tmux -L shots new-session -d -s rec -x 140 -y 38 'bonsai'
    tmux -L shots capture-pane -e -p -t rec > hero.ansi

Render it (Chrome headless does the painting):

    scripts/render_terminal_screenshot.py hero.ansi hero.png

`--html-only` writes the intermediate page instead, `--scale 1` disables the
retina 2x export, and `--no-frame` drops the rounded window chrome.
"""
from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
import unicodedata
from dataclasses import dataclass, replace
from pathlib import Path

DEFAULT_FG = "#ebe6d8"
DEFAULT_BG = "#0c0f0c"

FONT_SIZE = 13.0
CELL_W = 7.8      # Menlo advance is 0.6em
LINE_H = 17.0
PAD_X = 26.0
PAD_Y = 22.0
MARGIN = 26.0     # breathing room for the drop shadow
RADIUS = 12.0

CHROME_CANDIDATES = (
    "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    "/Applications/Chromium.app/Contents/MacOS/Chromium",
    "/usr/bin/google-chrome",
    "/usr/bin/chromium",
    "/usr/bin/chromium-browser",
)

PALETTE16 = (
    "#1c1f1c", "#c8705c", "#80b66c", "#c9a45c", "#6c98c8", "#a87fb8", "#6cb3b3", "#c4c0b4",
    "#3a4a3a", "#e08a76", "#9ccd86", "#e0c07c", "#8ab4e0", "#c79cd4", "#8ad0d0", "#ebe6d8",
)


@dataclass(frozen=True)
class Style:
    fg: str | None = None
    bg: str | None = None
    bold: bool = False
    dim: bool = False
    italic: bool = False
    underline: bool = False


def _rgb(params: list[str], start: int) -> tuple[str | None, int]:
    """Decode a 38/48 extended colour starting at params[start]; return (css, next)."""
    if start >= len(params):
        return None, start
    mode = params[start]
    if mode == "5" and start + 1 < len(params):
        index = int(params[start + 1]) % 256
        if index < 16:
            return PALETTE16[index], start + 2
        if index < 232:
            index -= 16
            levels = [0, 95, 135, 175, 215, 255]
            r, g, b = levels[index // 36], levels[(index % 36) // 6], levels[index % 6]
            return f"#{r:02x}{g:02x}{b:02x}", start + 2
        grey = 8 + (index - 232) * 10
        return f"#{grey:02x}{grey:02x}{grey:02x}", start + 2
    if mode == "2" and start + 3 < len(params):
        r, g, b = (max(0, min(255, int(value))) for value in params[start + 1:start + 4])
        return f"#{r:02x}{g:02x}{b:02x}", start + 4
    return None, start + 1


def apply_sgr(style: Style, raw: str) -> Style:
    params = [part for part in raw.replace(":", ";").split(";")] if raw else ["0"]
    if params == [""]:
        params = ["0"]
    i = 0
    while i < len(params):
        code = int(params[i] or 0)
        if code == 0:
            style = Style()
        elif code == 1:
            style = replace(style, bold=True)
        elif code == 2:
            style = replace(style, dim=True)
        elif code == 3:
            style = replace(style, italic=True)
        elif code == 4:
            style = replace(style, underline=True)
        elif code == 22:
            style = replace(style, bold=False, dim=False)
        elif code == 23:
            style = replace(style, italic=False)
        elif code == 24:
            style = replace(style, underline=False)
        elif code == 39:
            style = replace(style, fg=None)
        elif code == 49:
            style = replace(style, bg=None)
        elif 30 <= code <= 37:
            style = replace(style, fg=PALETTE16[code - 30])
        elif 90 <= code <= 97:
            style = replace(style, fg=PALETTE16[code - 90 + 8])
        elif 40 <= code <= 47:
            style = replace(style, bg=PALETTE16[code - 40])
        elif 100 <= code <= 107:
            style = replace(style, bg=PALETTE16[code - 100 + 8])
        elif code in (38, 48):
            colour, after = _rgb(params, i + 1)
            style = replace(style, **{"fg" if code == 38 else "bg": colour})
            i = after
            continue
        i += 1
    return style


def is_wide(char: str) -> bool:
    return unicodedata.east_asian_width(char) in ("W", "F")


@dataclass
class Screen:
    rows: list[dict[int, tuple[str, Style]]]
    width: int

    @property
    def height(self) -> int:
        return len(self.rows)


def parse_screen(text: str) -> Screen:
    rows: list[dict[int, tuple[str, Style]]] = [{}]
    row = col = 0
    width = 0
    style = Style()
    i = 0
    while i < len(text):
        char = text[i]
        if char == "\x1b":
            if text.startswith("\x1b[", i):
                end = i + 2
                while end < len(text) and not ("\x40" <= text[end] <= "\x7e"):
                    end += 1
                if end >= len(text):
                    break
                params, final = text[i + 2:end], text[end]
                if final == "m":
                    style = apply_sgr(style, params)
                elif final in "Hf":
                    fields = params.split(";") if params else []
                    row = (int(fields[0]) if fields and fields[0] else 1) - 1
                    col = (int(fields[1]) if len(fields) > 1 and fields[1] else 1) - 1
                    while len(rows) <= row:
                        rows.append({})
                i = end + 1
                continue
            if text.startswith("\x1b]", i):
                bel = text.find("\x07", i)
                st = text.find("\x1b\\", i)
                ends = [pos for pos in (bel, st) if pos != -1]
                i = (min(ends) + (2 if min(ends) == st else 1)) if ends else len(text)
                continue
            i += 2  # ESC(B, ESC=, ESC> and friends
            continue
        if char == "\n":
            row += 1
            col = 0
            while len(rows) <= row:
                rows.append({})
            i += 1
            continue
        if char == "\r":
            col = 0
            i += 1
            continue
        if char == "\t":
            col = (col // 8 + 1) * 8
            i += 1
            continue
        if char < " ":
            i += 1
            continue
        rows[row][col] = (char, style)
        step = 2 if is_wide(char) else 1
        col += step
        width = max(width, col)
        i += 1
    while rows and not rows[-1]:
        rows.pop()
    if not rows:
        raise SystemExit("capture contains no cells")
    return Screen(rows=rows, width=width)


def escape(char: str) -> str:
    return {"&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;"}.get(char, char)


# Font fallback renders box drawing and block elements with visible seams, so
# those cells become real geometry: one cell is one character cell, and a run of
# them joins into an unbroken line or a seamless block.
LIGHT = 1.15
HEAVY = 2.3

# char -> [(kind, weight)] with kinds h/v (full), hr/hl/vd/vu (half lines),
# rounded corners tl/tr/bl/br, and corners/square/t junctions.
BOX_STROKES: dict[str, list[tuple[str, float]]] = {}


def _stroke_group(weight: float, keys: str) -> list[tuple[str, float]]:
    return [(key, weight) for key in keys.split()]


for _weight, _glyphs in (
    (LIGHT, {
        "\u2500": "h", "\u2502": "v",
        "\u256d": "tl", "\u256e": "tr", "\u2570": "bl", "\u256f": "br",
        "\u250c": "hr vd", "\u2510": "hl vd", "\u2514": "hr vu", "\u2518": "hl vu",
        "\u251c": "v hr", "\u2524": "v hl", "\u252c": "h vd", "\u2534": "h vu",
        "\u253c": "h v",
        "\u2574": "hl", "\u2575": "vu", "\u2576": "hr", "\u2577": "vd",
    }),
    (HEAVY, {
        "\u2501": "h", "\u2503": "v",
        "\u250f": "hr vd", "\u2513": "hl vd", "\u2517": "hr vu", "\u251b": "hl vu",
        "\u2523": "v hr", "\u252b": "v hl", "\u2533": "h vd", "\u253b": "h vu",
        "\u254b": "h v",
    }),
    (LIGHT, {"\u2506": "dashv", "\u250a": "dashv", "\u2504": "dashh", "\u2508": "dashh"}),
):
    for _char, _keys in _glyphs.items():
        BOX_STROKES.setdefault(_char, []).extend(_stroke_group(_weight, _keys))

# char -> [(x, y, w, h) in cell fractions, opacity]
BLOCK_RECTS: dict[str, list[tuple[float, float, float, float, float]]] = {
    "\u2588": [(0, 0, 1, 1, 1.0)],
    "\u2580": [(0, 0, 1, 0.5, 1.0)],
    "\u2584": [(0, 0.5, 1, 0.5, 1.0)],
    "\u258c": [(0, 0, 0.5, 1, 1.0)],
    "\u2590": [(0.5, 0, 0.5, 1, 1.0)],
    "\u2596": [(0, 0.5, 0.5, 0.5, 1.0)],
    "\u2597": [(0.5, 0.5, 0.5, 0.5, 1.0)],
    "\u2598": [(0, 0, 0.5, 0.5, 1.0)],
    "\u259d": [(0.5, 0, 0.5, 0.5, 1.0)],
    "\u2591": [(0, 0, 1, 1, 0.25)],
    "\u2592": [(0, 0, 1, 1, 0.5)],
    "\u2593": [(0, 0, 1, 1, 0.75)],
    "\u258f": [(0, 0, 0.125, 1, 1.0)],
    "\u258e": [(0, 0, 0.25, 1, 1.0)],
    "\u258d": [(0, 0, 0.375, 1, 1.0)],
    "\u258b": [(0, 0, 0.625, 1, 1.0)],
    "\u258a": [(0, 0, 0.75, 1, 1.0)],
    "\u2589": [(0, 0, 0.875, 1, 1.0)],
    "\u2594": [(0, 0, 1, 0.125, 1.0)],
    "\u2581": [(0, 0.875, 1, 0.125, 1.0)],
    "\u2582": [(0, 0.75, 1, 0.25, 1.0)],
    "\u2583": [(0, 0.625, 1, 0.375, 1.0)],
    "\u2585": [(0, 0.375, 1, 0.625, 1.0)],
    "\u2586": [(0, 0.25, 1, 0.75, 1.0)],
    "\u2587": [(0, 0.125, 1, 0.875, 1.0)],
}


def geometry(char: str, left: float, top: float, colour: str) -> list[str]:
    """Return SVG primitives for one box-drawing or block cell, or [] if the
    character should stay a font glyph."""
    shapes: list[str] = []
    blocks = BLOCK_RECTS.get(char)
    if blocks is not None:
        for x, y, w, h, opacity in blocks:
            shapes.append(
                '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"{}/>'.format(
                    left + x * CELL_W,
                    top + y * LINE_H,
                    w * CELL_W,
                    h * LINE_H,
                    colour,
                    ' opacity="{}"'.format(opacity) if opacity != 1.0 else "",
                )
            )
        return shapes
    strokes = BOX_STROKES.get(char)
    if strokes is None:
        return shapes
    mid_x = left + CELL_W / 2
    mid_y = top + LINE_H / 2
    right = left + CELL_W
    bottom = top + LINE_H
    for kind, weight in strokes:
        if kind == "h":
            shapes.append(
                '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                    left, mid_y - weight / 2, CELL_W, weight, colour
                )
            )
        elif kind == "v":
            shapes.append(
                '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                    mid_x - weight / 2, top, weight, LINE_H, colour
                )
            )
        elif kind in ("hl", "hr"):
            x0, x1 = (left, mid_x) if kind == "hl" else (mid_x, right)
            shapes.append(
                '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                    x0, mid_y - weight / 2, x1 - x0, weight, colour
                )
            )
        elif kind in ("vu", "vd"):
            y0, y1 = (top, mid_y) if kind == "vu" else (mid_y, bottom)
            shapes.append(
                '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                    mid_x - weight / 2, y0, weight, y1 - y0, colour
                )
            )
        elif kind in ("tl", "tr", "bl", "br"):
            radius = min(CELL_W, LINE_H) / 2
            start, end, sweep = {
                "tl": ((right, mid_y), (mid_x, bottom), 0),
                "tr": ((left, mid_y), (mid_x, bottom), 1),
                "bl": ((right, mid_y), (mid_x, top), 1),
                "br": ((left, mid_y), (mid_x, top), 0),
            }[kind]
            shapes.append(
                '<path d="M {:.2f} {:.2f} A {:.2f} {:.2f} 0 0 {} {:.2f} {:.2f}" fill="none" '
                'stroke="{}" stroke-width="{:.2f}" stroke-linecap="butt"/>'.format(
                    start[0], start[1], radius, radius, sweep, end[0], end[1], colour, weight
                )
            )
        elif kind in ("dashv", "dashh"):
            if kind == "dashv":
                shapes.append(
                    '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                        mid_x - weight / 2, top, weight, LINE_H, colour
                    )
                )
            else:
                shapes.append(
                    '<rect x="{:.2f}" y="{:.2f}" width="{:.2f}" height="{:.2f}" fill="{}"/>'.format(
                        left, mid_y - weight / 2, CELL_W, weight, colour
                    )
                )
    return shapes


def render_html(screen: Screen, frame: bool) -> str:
    cell_bg_default = DEFAULT_BG
    blocks: list[str] = []
    overlay: list[str] = []
    for row_index, cells in enumerate(screen.rows):
        top = row_index * LINE_H
        run_start = 0
        run_colour: str | None = None
        for col in range(screen.width + 1):
            cell = cells.get(col)
            colour = (cell[1].bg or cell_bg_default) if cell else cell_bg_default
            if colour != run_colour:
                if run_colour is not None and col > run_start:
                    blocks.append(
                        '<b style="left:{:.2f}px;top:{:.2f}px;width:{:.2f}px;height:{:.2f}px;'
                        'background:{}"></b>'.format(
                            run_start * CELL_W, top, (col - run_start) * CELL_W, LINE_H, run_colour
                        )
                    )
                run_start, run_colour = col, colour
        for col, (char, style) in sorted(cells.items()):
            if char == " ":
                continue
            ink = style.fg or DEFAULT_FG
            shapes = geometry(char, col * CELL_W, top, ink)
            if shapes:
                if style.dim:
                    overlay.append('<g opacity="0.62">' + "".join(shapes) + "</g>")
                else:
                    overlay.extend(shapes)
                continue
            css = ["left:{:.2f}px".format(col * CELL_W), "top:{:.2f}px".format(top)]
            css.append("color:" + ink)
            if style.bold:
                css.append("font-weight:700")
            if style.dim:
                css.append("opacity:0.62")
            if style.italic:
                css.append("font-style:italic")
            if style.underline:
                css.append("text-decoration:underline")
            blocks.append('<i style="{}">{}</i>'.format(";".join(css), escape(char)))

    content_w = screen.width * CELL_W
    content_h = screen.height * LINE_H
    pad = (PAD_X, PAD_Y) if frame else (0.0, 0.0)
    radius = RADIUS if frame else 0.0
    shadow = "0 26px 64px rgba(0,0,0,0.52)" if frame else "none"
    page_w = content_w + 2 * pad[0] + 2 * MARGIN
    page_h = content_h + 2 * pad[1] + 2 * MARGIN
    return f"""<!doctype html>
<html><head><meta charset="utf-8"><style>
  html,body{{margin:0;padding:0;background:transparent}}
  #page{{position:relative;width:{page_w:.0f}px;height:{page_h:.0f}px}}
  #term{{position:absolute;left:{MARGIN:.0f}px;top:{MARGIN:.0f}px;
        width:{content_w + 2 * pad[0]:.0f}px;height:{content_h + 2 * pad[1]:.0f}px;
        background:{DEFAULT_BG};border-radius:{radius:.0f}px;box-shadow:{shadow};
        overflow:hidden}}
  #grid{{position:absolute;left:{pad[0]:.0f}px;top:{pad[1]:.0f}px;
        width:{content_w:.0f}px;height:{content_h:.0f}px}}
  #grid b,#grid i{{position:absolute;display:block}}
  #geo{{position:absolute;left:0;top:0;overflow:visible}}
  #grid i{{width:{CELL_W:.2f}px;height:{LINE_H:.2f}px;text-align:center;
          font-family:Menlo,"SF Mono","DejaVu Sans Mono",monospace;
          font-size:{FONT_SIZE:.1f}px;line-height:{LINE_H:.1f}px;font-style:normal;
          white-space:pre;overflow:visible}}
</style></head><body><div id="page"><div id="term"><div id="grid">
{''.join(blocks)}
<svg id="geo" width="{content_w:.2f}" height="{content_h:.2f}" viewBox="0 0 {content_w:.2f} {content_h:.2f}">
{''.join(overlay)}
</svg>
</div></div></div></body></html>
"""


def find_chrome() -> str:
    for candidate in CHROME_CANDIDATES:
        if Path(candidate).exists():
            return candidate
    found = shutil.which("google-chrome") or shutil.which("chromium")
    if not found:
        raise SystemExit("no Chrome/Chromium found; pass --html-only and render manually")
    return found


def page_size(screen: Screen, frame: bool) -> tuple[int, int]:
    pad = (PAD_X, PAD_Y) if frame else (0.0, 0.0)
    return (
        round(screen.width * CELL_W + 2 * pad[0] + 2 * MARGIN),
        round(screen.height * LINE_H + 2 * pad[1] + 2 * MARGIN),
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("capture", type=Path, help="ANSI capture from tmux capture-pane -e")
    parser.add_argument("png", type=Path, help="output PNG path")
    parser.add_argument("--html-only", action="store_true", help="write .html instead of a PNG")
    parser.add_argument("--scale", type=int, default=2, help="device scale factor (default 2)")
    parser.add_argument("--no-frame", action="store_true", help="no padding, radius, or shadow")
    args = parser.parse_args(argv)

    screen = parse_screen(args.capture.read_bytes().decode("utf-8", "replace"))
    html = render_html(screen, frame=not args.no_frame)
    html_path = args.png.with_suffix(".html")
    html_path.write_text(html, encoding="utf-8")
    if args.html_only:
        print(html_path)
        return 0

    width, height = page_size(screen, not args.no_frame)
    args.png.parent.mkdir(parents=True, exist_ok=True)
    command = [
        find_chrome(),
        "--headless=new",
        "--disable-gpu",
        "--hide-scrollbars",
        "--default-background-color=00000000",
        "--force-device-scale-factor={}".format(args.scale),
        "--virtual-time-budget=2000",
        "--window-size={},{}".format(width, height),
        "--screenshot={}".format(args.png.resolve()),
        html_path.resolve().as_uri(),
    ]
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode != 0 or not args.png.exists():
        sys.stderr.write(result.stdout + result.stderr)
        raise SystemExit("chrome failed to render {}".format(args.png))
    html_path.unlink(missing_ok=True)
    print(args.png, "{}x{} @{}x".format(width, height, args.scale))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
