"""Latency vs. confirmed findings: the stage 4 Pareto frontiers on one chart.

Usage: uv run --with matplotlib python3 plot_pareto.py <out-dir> [--results results/stage4]

Reads pareto.csv (all confirmed problems) and pareto-sev-high-medium.csv
(confirmed medium or high) written by pareto.py, draws each frontier as a line,
and adds Greptile Base ($1) and Plus ($3) alone for both measures even though
they are off the frontier. Points are numbered; one option keeps its number on
both lines, and the key names each option. No metric is recomputed here.
Adapted from session b8556873's /tmp/review-eval/pareto/plot.py (label placement).
"""
import argparse
import csv
import os
import pathlib

os.environ.setdefault("MPLCONFIGDIR", "/tmp/review-arms-mpl")
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D
from matplotlib.ticker import FixedLocator, FuncFormatter, NullLocator
from matplotlib.transforms import Bbox

p = argparse.ArgumentParser()
p.add_argument("out")
p.add_argument("--results", default=str(pathlib.Path(__file__).resolve().parent / "results/stage4"))
a = p.parse_args()
res, out = pathlib.Path(a.results), pathlib.Path(a.out)
out.mkdir(parents=True, exist_ok=True)

def load(name):
    return {r["option"]: r for r in csv.DictReader(open(res / name))}

SERIES = [("All confirmed", load("pareto.csv"), "#2a78d6"),
          ("Medium or high only", load("pareto-sev-high-medium.csv"), "#eb6834")]
EXTRA = {"Greptile/base": "Greptile Base ($1 per review)", "Greptile/plus": "Greptile Plus ($3 per review)"}

def label(option):
    return option.replace("Greptile/base", "Greptile Base").replace("Greptile/plus", "Greptile Plus")

# One number per option, ordered by latency across both frontiers; Greptile alone last.
frontier = {o: float(r["median_latency_s"]) for _, rows, _ in SERIES for o, r in rows.items() if r["frontier"] == "True"}
order = sorted(frontier, key=lambda o: (frontier[o], o)) + list(EXTRA)
number = {o: i for i, o in enumerate(order, 1)}

plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 12, "svg.fonttype": "none"})
BG, INK, MUTED, GRID = "#fcfcfb", "#0b0b0b", "#52514e", "#e6e6e3"
fig = plt.figure(figsize=(20, 12.5), facecolor=BG)
fig.text(.05, .95, "Review latency vs. confirmed bugs found", fontsize=24, fontweight="bold", color=INK)
fig.text(.05, .918, "Pareto frontiers over combinations of 1–3 parallel reviewers · 70 historical mjolnir diffs · "
         "blind Opus/high verification (verify5)", fontsize=13.5, color=MUTED)
ax = fig.add_axes([.06, .1, .53, .77], facecolor=BG)
key = fig.add_axes([.63, .08, .36, .81])
key.set_axis_off()

ax.set_xscale("log")
ax.set_xlim(38, 320)
ax.set_ylim(40, 250)
ax.xaxis.set_major_locator(FixedLocator([40, 50, 60, 80, 100, 120, 150, 200, 250, 300]))
ax.xaxis.set_major_formatter(FuncFormatter(lambda x, _: f"{x:g}"))
ax.xaxis.set_minor_locator(NullLocator())
ax.set_xlabel("Median review latency, slowest reviewer (seconds, log scale)", labelpad=14, fontsize=13, color=INK)
ax.set_ylabel("Confirmed bugs found across 70 diffs", labelpad=12, fontsize=13, color=INK)
ax.spines[["top", "right"]].set_visible(False)
for s in ("left", "bottom"):
    ax.spines[s].set_color("#b9b8b2")
ax.tick_params(colors=MUTED, length=4, pad=7)
ax.grid(axis="y", color=GRID, linewidth=.7)
ax.set_axisbelow(True)

points = []  # (x, y, option, color)
for name, rows, color in SERIES:
    pts = sorted(((float(r["median_latency_s"]), float(r["findings"]), o) for o, r in rows.items()
                  if r["frontier"] == "True"))
    ax.plot([x for x, _, _ in pts], [y for _, y, _ in pts], color=color, linewidth=2, zorder=2,
            drawstyle="steps-post")
    ax.scatter([x for x, _, _ in pts], [y for _, y, _ in pts], s=64, color=color, edgecolor=BG,
               linewidth=2, zorder=3)
    points += [(x, y, o, color) for x, y, o in pts]
    for o in EXTRA:
        r = rows[o]
        x, y = float(r["median_latency_s"]), float(r["findings"])
        ax.scatter([x], [y], s=110, marker="D", facecolor=BG, edgecolor=color, linewidth=2, zorder=3)
        points.append((x, y, o, color))

# Numbered labels placed in display space so no label covers a point or another label.
fig.canvas.draw()
renderer = fig.canvas.get_renderer()
point_boxes = []
for x, y, _, _ in points:
    px, py = ax.transData.transform((x, y))
    point_boxes.append(Bbox.from_extents(px - 6, py - 6, px + 6, py + 6))
box = ax.get_window_extent(renderer)
safe = Bbox.from_extents(box.x0 + 3, box.y0 + 3, box.x1 - 3, box.y1 - 3)
occupied = []
offsets = [(r * dx, r * dy) for r in (13, 22, 32, 44, 58, 74)
           for dx, dy in ((0, 1), (-.8, .8), (.8, .8), (-1, 0), (1, 0), (-.8, -.8), (.8, -.8), (0, -1))]
for x, y, o, color in points:
    for dx, dy in offsets:
        ann = ax.annotate(str(number[o]), xy=(x, y), xytext=(dx, dy), textcoords="offset points",
                          ha="center", va="center", fontsize=10.5, color=INK, zorder=5,
                          bbox={"facecolor": BG, "edgecolor": "none", "pad": .3})
        b = ann.get_window_extent(renderer).expanded(1.15, 1.2)
        if safe.contains(b.x0, b.y0) and safe.contains(b.x1, b.y1) and \
                not any(b.overlaps(c) for c in occupied + point_boxes):
            occupied.append(b)
            if abs(dx) + abs(dy) > 14:
                ax.annotate("", xy=(x, y), xytext=(dx, dy), textcoords="offset points", zorder=4,
                            arrowprops={"arrowstyle": "-", "color": "#a3a29b", "linewidth": .6,
                                        "shrinkA": 7, "shrinkB": 5})
            break
        ann.remove()
    else:
        raise RuntimeError(f"no free place for the label of {o} at ({x}, {y})")

ax.legend(handles=[Line2D([], [], color=c, linewidth=2, marker="o", markersize=7, markeredgecolor=BG, label=n)
                   for n, _, c in SERIES] +
          [Line2D([], [], linestyle="none", marker="D", markersize=8, markerfacecolor=BG,
                  markeredgecolor=MUTED, markeredgewidth=2, label="Greptile alone (off the frontier)")],
          loc="lower right", frameon=False, fontsize=12, labelcolor=INK)

# Key: number, option, latency and both counts (blank where the option is off that frontier).
key.text(0, 1.0, "Reviewers", fontsize=15, fontweight="bold", color=INK, va="bottom", transform=key.transAxes)
key.text(.70, 1.0, "s", fontsize=11, color=MUTED, ha="right", va="bottom", transform=key.transAxes)
key.text(.86, 1.0, "all", fontsize=11, color=MUTED, ha="right", va="bottom", transform=key.transAxes)
key.text(1.0, 1.0, "med+", fontsize=11, color=MUTED, ha="right", va="bottom", transform=key.transAxes)
step = .965 / len(order)
for i, o in enumerate(order):
    yy = .975 - i * step
    if o in EXTRA:
        name = EXTRA[o]
        if o == list(EXTRA)[0]:
            key.plot([0, 1], [yy + step * .55] * 2, color=GRID, linewidth=.8, transform=key.transAxes)
    else:
        name = label(o)
    lat = float(SERIES[0][1][o]["median_latency_s"])
    cells = [f"{number[o]:>2}", name, f"{lat:.0f}"]
    key.text(0, yy, cells[0], fontsize=11, fontweight="bold", color=INK, va="center", transform=key.transAxes)
    key.text(.06, yy, cells[1], fontsize=11, color=INK, va="center", transform=key.transAxes)
    key.text(.70, yy, cells[2], fontsize=11, color=MUTED, ha="right", va="center", transform=key.transAxes)
    for (sname, rows, color), xx in zip(SERIES, (.86, 1.0)):
        r = rows[o]
        shown = r["frontier"] == "True" or o in EXTRA
        key.text(xx, yy, f"{float(r['findings']):.0f}" if shown else "", fontsize=11, color=INK, ha="right",
                 va="center", transform=key.transAxes)

fig.text(.05, .025, "Latency: median over diffs of the slowest reviewer's own turn. Counts: distinct confirmed "
         "problems, exact mean over assignments of distinct sessions. Greptile's context could not be pinned to "
         "each diff's commit; any leak only helps it.", fontsize=10.5, color=MUTED)
for ext in ("png", "svg"):
    fig.savefig(out / f"review-pareto-latency-findings.{ext}", dpi=150, facecolor=BG)
print(out / "review-pareto-latency-findings.png")
