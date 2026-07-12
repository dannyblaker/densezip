#!/usr/bin/env python3
"""Generate the README benchmark chart (assets/benchmarks-{light,dark}.svg).

One row per corpus file: the bar length is how much smaller densezip's
output is than the best competitor's (best of gzip/bzip2/xz/zstd/7z chosen
per file) — longer bar = bigger win, and the label restates the bar. Data
is copied from BENCHMARKS.md — re-run this script after updating it.
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
        muted="#898781",
        fill="#2a78d6",
        grid="#e1e0d9",
        baseline="#c3c2b7",
    ),
    "dark": dict(
        primary="#ffffff",
        secondary="#c3c2b7",
        muted="#898781",
        fill="#3987e5",
        grid="#2c2c2a",
        baseline="#383835",
    ),
}

FONT = "-apple-system,BlinkMacSystemFont,'Segoe UI','Noto Sans',Helvetica,Arial,sans-serif"
W = 920
NAME_X = 176  # right edge of the file-name column
BAR_X = 184
SCALE_MAX = 80.0  # % axis; largest margin is 79.2
BAR_W = 660  # width of the 0..SCALE_MAX plot area
BAR_H = 14
ROW_H = 26
MINUS = "−"


def px(pct):
    return BAR_W * pct / SCALE_MAX


def bar_path(x, y, w, h, r=4):
    """Left end square at the baseline, right (data) end rounded."""
    if w < 2 * r:  # sliver bar: too narrow to round, keep it visible
        return f"M{x},{y} h{max(w, 1.5):.1f} v{h} h-{max(w, 1.5):.1f} z"
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
    out.append(text(16, y, "How much smaller densezip's output is, per file", 16,
                    t["primary"], "600"))
    y += 22
    out.append(text(16, y, "Reduction vs the best (smallest) output of "
                    f"gzip {MINUS}9 · bzip2 {MINUS}9 · xz {MINUS}9e · zstd {MINUS}22 · "
                    f"7z {MINUS}mx=9, chosen per file — a stricter baseline "
                    "than any single tool.", 12, t["secondary"]))
    y += 18
    out.append(text(16, y, "Longer bar = bigger win. densezip wins on every file; "
                    "every archive is verified to reconstruct bit-exactly.",
                    12, t["secondary"]))
    y += 30

    # axis tick labels, once, above the plot
    for tick in range(0, int(SCALE_MAX) + 1, 20):
        anchor = "start" if tick == 0 else "middle"
        out.append(text(BAR_X + px(tick), y, f"{MINUS}{tick}%" if tick else "0",
                        11, t["muted"], anchor=anchor))
    y += 12
    grid_top = y

    body_rows = []
    for title, total, rows in SECTIONS:
        y += 20
        body_rows.append(text(16, y, title, 13, t["primary"], "600"))
        body_rows.append(text(BAR_X + BAR_W + 50, y, total, 13, t["primary"], "600", "end"))
        y += 12
        for name, margin in rows:
            body_rows.append(text(NAME_X, y + 11, name, 12.5, t["secondary"], anchor="end"))
            body_rows.append(f'<path d="{bar_path(BAR_X, y, px(margin), BAR_H)}" '
                             f'fill="{t["fill"]}"/>')
            body_rows.append(text(BAR_X + px(margin) + 6, y + 11, f"{MINUS}{margin}%",
                                  12, t["primary"], "600"))
            y += ROW_H
        y += 12
    grid_bottom = y - ROW_H + BAR_H + 12 - 12

    # gridlines behind the bars, baseline slightly stronger
    for tick in range(0, int(SCALE_MAX) + 1, 20):
        color = t["baseline"] if tick == 0 else t["grid"]
        x = BAR_X + px(tick)
        out.append(f'<line x1="{x:.1f}" y1="{grid_top}" x2="{x:.1f}" '
                   f'y2="{grid_bottom}" stroke="{color}" stroke-width="1"/>')
    out.extend(body_rows)

    h = y + 4
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
