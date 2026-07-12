#!/usr/bin/env python3
"""Generate the README benchmark chart (assets/benchmarks-{light,dark}.svg).

One meter-style row per corpus file: the full track is the best competitor's
output (100%, best of gzip/bzip2/xz/zstd/7z chosen per file), the filled bar
is densezip's output as a share of it, and the label is the margin. Data is
copied from BENCHMARKS.md — re-run this script after updating it.
"""

# (file, densezip margin vs best competitor, % — from BENCHMARKS.md)
REAL = [
    ("sample.png", 79.2),
    ("sample.pdf", 56.2),
    ("inventory.db", 52.7),
    ("src.tar.gz", 34.2),
    ("report.docx", 26.7),
    ("sales.csv", 21.5),
    ("photo.jpg", 21.0),
    ("photo.png", 1.0),
]
SILESIA = [
    ("webster", 27.5),
    ("ooffice", 23.9),
    ("dickens", 21.3),
    ("samba", 20.0),
    ("reymont", 19.8),
    ("osdb", 17.5),
    ("xml", 16.6),
    ("sao", 12.8),
    ("mozilla", 10.0),
    ("x-ray", 6.3),
    ("mr", 5.3),
    ("nci", 0.1),
]
SECTIONS = [
    ("Real-world files", "total −8.9%", REAL),
    ("Silesia corpus (212 MB standard benchmark)", "total −15.4%", SILESIA),
]

THEMES = {
    # GitHub README surfaces: light #ffffff, dark #0d1117 (background stays
    # transparent; inks and the blue ramp are validated per surface)
    "light": dict(
        primary="#0b0b0b",
        secondary="#52514e",
        fill="#2a78d6",
        track="#cde2fb",
    ),
    "dark": dict(
        primary="#ffffff",
        secondary="#c3c2b7",
        fill="#3987e5",
        track="#0d366b",
    ),
}

FONT = "-apple-system,BlinkMacSystemFont,'Segoe UI','Noto Sans',Helvetica,Arial,sans-serif"
W = 920
NAME_X = 176  # right edge of the file-name column
BAR_X = 184
BAR_W = 660
BAR_H = 14
ROW_H = 26
MINUS = "−"


def bar_path(x, y, w, h, r=4):
    """Left end square at the baseline, right (data) end rounded."""
    return (
        f"M{x},{y} h{w - r:.1f} a{r},{r} 0 0 1 {r},{r} v{h - 2 * r} "
        f"a{r},{r} 0 0 1 -{r},{r} h-{w - r:.1f} z"
    )


def text(x, y, s, size, color, weight="normal", anchor="start"):
    return (
        f'<text x="{x}" y="{y}" font-size="{size}" fill="{color}" '
        f'font-weight="{weight}" text-anchor="{anchor}">{s}</text>'
    )


def render(theme):
    t = THEMES[theme]
    out = []
    y = 30
    out.append(text(16, y, "densezip vs the best competitor, per file", 16, t["primary"], "600"))
    y += 22
    out.append(text(16, y, "Compressed size as a share of the best (smallest) output "
                    f"of gzip {MINUS}9 · bzip2 {MINUS}9 · xz {MINUS}9e · zstd {MINUS}22 · "
                    f"7z {MINUS}mx=9, chosen per file.", 12, t["secondary"]))
    y += 18
    out.append(text(16, y, "Full track = best competitor (100%) · bar = densezip · "
                    "labels = densezip's margin · every archive verified bit-exact.",
                    12, t["secondary"]))
    y += 36

    for title, total, rows in SECTIONS:
        out.append(text(16, y, title, 13, t["primary"], "600"))
        out.append(text(BAR_X + BAR_W, y, total, 13, t["primary"], "600", "end"))
        y += 12
        for name, margin in rows:
            pct = 100.0 - margin
            fill_w = BAR_W * pct / 100.0
            out.append(text(NAME_X, y + 11, name, 12.5, t["secondary"], anchor="end"))
            out.append(f'<path d="{bar_path(BAR_X, y, BAR_W, BAR_H)}" fill="{t["track"]}"/>')
            out.append(f'<path d="{bar_path(BAR_X, y, fill_w, BAR_H)}" fill="{t["fill"]}"/>')
            label = f"{MINUS}{margin}%"
            est = 7.4 * len(label) + 12  # measured-enough for 12px semibold digits
            gap = BAR_W - fill_w
            lx = BAR_X + fill_w + 6 if gap >= est else BAR_X + BAR_W + 6
            out.append(text(lx, y + 11, label, 12, t["primary"], "600"))
            y += ROW_H
        y += 28

    h = y - 8
    body = "\n".join(out)
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{h}" '
        f'viewBox="0 0 {W} {h}" font-family="{FONT}" '
        f'role="img" aria-label="densezip beats the best competitor on all 20 benchmark files">'
        f"\n{body}\n</svg>\n"
    )


if __name__ == "__main__":
    import pathlib

    root = pathlib.Path(__file__).resolve().parent.parent / "assets"
    root.mkdir(exist_ok=True)
    for theme in THEMES:
        path = root / f"benchmarks-{theme}.svg"
        path.write_text(render(theme))
        print(f"wrote {path}")
