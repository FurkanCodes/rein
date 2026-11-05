"""Turns a terminal screen, as `tmux capture-pane -e -p` prints it, into an
HTML page that draws the same text in the same colours.

Usage: python3 render.py screen.ansi [--rows 0:3] [--cols 1:19] [--clear|--window] > screen.html
--rows and --cols keep only part of the screen; --clear draws it with no
background and no margin; --window draws it in a window with a title bar.
Prints the page's width and height in pixels to stderr, for the browser
that photographs it. demo.py uses page() itself.
"""

import html
import math
import re
import sys

FONT_PX = 14
LINE_PX = 17
# Menlo's advance width, in ems.
ADVANCE = 0.6021
PAD_X, PAD_Y = 22, 18
BACKGROUND, FOREGROUND = "#15171c", "#d7dae0"
# The 16 colours, dark then bright.
SIXTEEN = [
    "#2a2d35", "#e06c75", "#98c379", "#e5c07b", "#61afef", "#c678dd", "#56b6c2", "#c8ccd4",
    "#5c6370", "#ef8891", "#b5e08f", "#f0d49b", "#82c4ff", "#d99cec", "#7fd3dc", "#ffffff",
]


def colour_256(n):
    if n < 16:
        return SIXTEEN[n]
    if n < 232:
        n -= 16
        levels = [0, 95, 135, 175, 215, 255]
        r, g, b = levels[n // 36], levels[(n // 6) % 6], levels[n % 6]
        return f"#{r:02x}{g:02x}{b:02x}"
    grey = 8 + 10 * (n - 232)
    return f"#{grey:02x}{grey:02x}{grey:02x}"


class Style:
    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False

    def apply(self, params):
        codes = [int(p) if p else 0 for p in params.split(";")] if params else [0]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                self.reset()
            elif c == 1:
                self.bold = True
            elif c == 2:
                self.dim = True
            elif c == 3:
                self.italic = True
            elif c == 4:
                self.underline = True
            elif c == 7:
                self.reverse = True
            elif c == 22:
                self.bold = self.dim = False
            elif c == 23:
                self.italic = False
            elif c == 24:
                self.underline = False
            elif c == 27:
                self.reverse = False
            elif 30 <= c <= 37:
                self.fg = SIXTEEN[c - 30]
            elif 90 <= c <= 97:
                self.fg = SIXTEEN[c - 90 + 8]
            elif 40 <= c <= 47:
                self.bg = SIXTEEN[c - 40]
            elif 100 <= c <= 107:
                self.bg = SIXTEEN[c - 100 + 8]
            elif c == 39:
                self.fg = None
            elif c == 49:
                self.bg = None
            elif c in (38, 48) and i + 1 < len(codes):
                if codes[i + 1] == 5 and i + 2 < len(codes):
                    value = colour_256(codes[i + 2])
                    i += 2
                elif codes[i + 1] == 2 and i + 4 < len(codes):
                    value = "#{:02x}{:02x}{:02x}".format(*codes[i + 2 : i + 5])
                    i += 4
                else:
                    value = None
                if c == 38:
                    self.fg = value
                else:
                    self.bg = value
                i += 1
            i += 1

    def css(self):
        fg, bg = self.fg, self.bg
        if self.reverse:
            fg, bg = bg or BACKGROUND, fg or FOREGROUND
        rules = []
        if fg:
            rules.append(f"color:{fg}")
        if bg:
            rules.append(f"background:{bg}")
        if self.bold:
            rules.append("font-weight:700")
        if self.dim:
            rules.append("opacity:.62")
        if self.italic:
            rules.append("font-style:italic")
        if self.underline:
            rules.append("text-decoration:underline")
        return ";".join(rules)


ESCAPE = re.compile(r"\x1b\[([0-9;]*)([A-Za-z])")


def render(text, rows=None, cols=None):
    """The screen's lines as HTML, and how many columns they take: only
    `rows` and `cols` of it (start, end) where given, colours kept."""
    lines = text.rstrip("\n").split("\n")
    while lines and not ESCAPE.sub("", lines[-1]).strip():
        lines.pop()
    style = Style()
    out, widest = [], 0
    for n, line in enumerate(lines):
        # Escapes on rows left out still set the colours of those after.
        keep = rows is None or rows[0] <= n < rows[1]
        pos, col, parts = 0, 0, []
        for match in [*ESCAPE.finditer(line), None]:
            end = match.start() if match else len(line)
            for ch in line[pos:end]:
                if cols is None or cols[0] <= col < cols[1]:
                    parts.append((style.css(), ch))
                col += 1
            if match is None:
                break
            if match.group(2) == "m":
                style.apply(match.group(1))
            pos = match.end()
        if keep:
            widest = max(widest, len(parts))
            out.append("".join(f'<span style="{css}">{html.escape(t)}</span>' if css else html.escape(t) for css, t in parts))
    return out, widest or 80


def span(text):
    """`3:7` as (3, 7)."""
    start, end = text.split(":")
    return int(start), int(end)


BAR_PX = 30


def page(text, rows=None, cols=None, clear=False, window=False, size=None):
    """The HTML page for a screen, and its width and height in pixels.
    `clear`: no background and no margin, for the logo. `window`: in a
    window with rounded corners and a title bar, outside it transparent.
    `size`: (columns, rows) the page shows whatever the screen holds, so
    the frames of a recording are all one size."""
    lines, width_cols = render(text, rows, cols)
    if size:
        width_cols = max(width_cols, size[0])
        lines += [""] * (size[1] - len(lines))
    pad_x, pad_y, background = (0, 0, "transparent") if clear else (PAD_X, PAD_Y, BACKGROUND)
    width = math.ceil(width_cols * FONT_PX * ADVANCE) + 2 * pad_x
    height = len(lines) * LINE_PX + 2 * pad_y
    pre = f"<pre>{chr(10).join(lines)}</pre>"
    style = f"""html, body {{ margin: 0; background: {background}; }}
pre {{ margin: 0; padding: {pad_y}px {pad_x}px; color: {FOREGROUND};
  font: {FONT_PX}px/{LINE_PX}px Menlo, "SF Mono", monospace; }}"""
    if window:
        height += BAR_PX
        style = f"""html, body {{ margin: 0; background: transparent; }}
.window {{ width: {width}px; height: {height}px; box-sizing: border-box; overflow: hidden;
  background: {BACKGROUND}; border-radius: 12px; border: 1px solid #2c3038; }}
.bar {{ height: {BAR_PX}px; display: flex; align-items: center; gap: 8px; padding: 0 14px;
  background: #1c1f25; border-bottom: 1px solid #2c3038; }}
.bar i {{ width: 12px; height: 12px; border-radius: 50%; display: block; }}
pre {{ margin: 0; padding: {pad_y}px {pad_x}px; color: {FOREGROUND};
  font: {FONT_PX}px/{LINE_PX}px Menlo, "SF Mono", monospace; }}"""
        pre = f"""<div class="window"><div class="bar"><i style="background:#ff5f57"></i><i style="background:#febc2e"></i><i style="background:#28c840"></i></div>{pre}</div>"""
    return f"""<!doctype html><meta charset="utf-8">\n<style>\n{style}\n</style>\n{pre}""", width, height


def main():
    args = sys.argv[1:]
    rows = span(args[args.index("--rows") + 1]) if "--rows" in args else None
    cols = span(args[args.index("--cols") + 1]) if "--cols" in args else None
    with open(args[0], encoding="utf-8") as f:
        html_page, width, height = page(f.read(), rows, cols, clear="--clear" in args, window="--window" in args)
    print(html_page)
    print(f"{width} {height}", file=sys.stderr)


if __name__ == "__main__":
    main()
