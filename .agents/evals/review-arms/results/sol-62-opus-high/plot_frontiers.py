"""Plot the audited verify9 latency and cost frontiers."""
import argparse
import csv
from io import StringIO
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.lines import Line2D
from matplotlib.path import Path as PlotPath
from matplotlib.ticker import FixedLocator, FuncFormatter, NullLocator
from matplotlib.transforms import Bbox

SERIES = [
    ("All confirmed", "pareto.csv", "#2a78d6"),
    ("Medium or high", "pareto-sev-high-medium.csv", "#eb6834"),
    ("High only", "pareto-sev-high.csv", "#1baf7a"),
]
EXTRAS = {"Greptile/base": "Greptile Base", "Greptile/plus": "Greptile Plus"}
BG, INK, MUTED, GRID = "#fcfcfb", "#0b0b0b", "#52514e", "#e6e6e3"


def load_points(results, filename, axis, flag):
    with (results / filename).open(newline="") as source:
        rows = list(csv.DictReader(source))
    points = {
        row["option"]: (float(row[axis]), float(row["findings"]), row[flag] == "True")
        for row in rows if row[axis]
    }
    eligible = {option: point for option, point in points.items() if "Greptile/" not in option}
    removed = [point for option, point in points.items() if option not in eligible and point[2]]
    retained = [point for point in eligible.values() if point[2]]
    for option, (x, y, flagged) in eligible.items():
        # By transitivity, every dominated point has a dominator on the old frontier.
        # Weak inequalities on rounded coordinates admit possible dominance.
        # Two strict inequalities certify dominance at the original precision.
        if (not flagged and any(rx <= x and ry >= y for rx, ry, _ in removed)
                and not any(kx < x and ky > y for kx, ky, _ in retained)):
            raise ValueError(f"{filename}: {flag} for {option} needs full-precision verification")
    return eligible | {option: (*points[option][:2], False) for option in EXTRAS}


def load_key_values(results):
    values = {}
    for name, filename, _ in SERIES:
        with (results / filename).open(newline="") as source:
            rows = csv.DictReader(source)
            values[name] = {row["option"]: row for row in rows}
    return values


def name_option(option):
    return (EXTRAS.get(option, option).replace("gpt-6.2-sol/", "Sol 6.2/")
            .replace("Terra/", "Terra 6.1/").replace("Sol/", "Sol 6.1/")
            .replace("Gemini/", "Hydrogen/"))


def place_numbers(fig, ax, marks, number):
    fig.canvas.draw()
    renderer = fig.canvas.get_renderer()
    box = ax.get_window_extent(renderer)
    safe = Bbox.from_extents(box.x0 + 4, box.y0 + 4, box.x1 - 4, box.y1 - 4)
    occupied = []
    label_boxes = []
    leaders = []
    for x, y, _, _ in marks:
        px, py = ax.transData.transform((x, y))
        occupied.append(Bbox.from_extents(px - 7, py - 7, px + 7, py + 7))
    offsets = [
        (radius * dx, radius * dy)
        for radius in (14, 21, 30, 41, 54, 68, 83)
        for dx, dy in ((0, 1), (-.6, .9), (.6, .9), (0, -1),
                       (-.6, -.9), (.6, -.9), (1, .35), (1, -.35), (-1, .35), (-1, -.35))
    ]
    for x, y, option, color in marks:
        for dx, dy in offsets:
            label = ax.annotate(
                str(number[option]), (x, y), xytext=(dx, dy),
                textcoords="offset points", ha="center", va="center",
                fontsize=10.5, color=INK, zorder=5,
                bbox={"facecolor": BG, "edgecolor": "none", "pad": .4},
            )
            bounds = label.get_window_extent(renderer).expanded(1.18, 1.3)
            start = ax.transData.transform((x, y))
            finish = ((bounds.x0 + bounds.x1) / 2, (bounds.y0 + bounds.y1) / 2)
            leader = PlotPath([start, finish])
            if (safe.contains(bounds.x0, bounds.y0)
                    and safe.contains(bounds.x1, bounds.y1)
                    and not any(bounds.overlaps(other) for other in occupied)
                    and not any(leader.intersects_bbox(other) for other in label_boxes)
                    and not any(other.intersects_bbox(bounds) for other in leaders)):
                occupied.append(bounds)
                label_boxes.append(bounds)
                leaders.append(leader)
                ax.annotate(
                    "", (x, y), xytext=(dx, dy), textcoords="offset points",
                    zorder=4, arrowprops={"arrowstyle": "-", "color": color,
                                          "alpha": .65, "linewidth": .75,
                                          "shrinkA": 7, "shrinkB": 4},
                )
                break
            label.remove()
        else:
            raise RuntimeError(f"No label space for {option} at {(x, y)}")


def render(results, output, *, cost):
    axis, flag = ("cost_per_diff_usd", "cost_frontier") if cost else ("median_latency_s", "frontier")
    series = [(name, color, load_points(results, filename, axis, flag))
              for name, filename, color in SERIES]
    reference = series[0][2]
    key_values = load_key_values(results)
    order = sorted({o for _, _, points in series for o, p in points.items() if p[2]},
                   key=lambda o: (reference[o][0], o)) + list(EXTRAS)
    number = {o: i for i, o in enumerate(order, 1)}

    plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 12,
                         "svg.fonttype": "none", "svg.hashsalt": "review-arms-verify9"})
    measure = "cost" if cost else "latency"
    fig = plt.figure(figsize=(25, 14), facecolor=BG)
    fig.text(.05, .95, f"Review {measure} vs. confirmed bugs found",
             fontsize=25, fontweight="bold", color=INK)
    fig.text(.05, .918,
             "1–3 parallel reviewers · Greptile solo references only"
             " · 70 historical mjolnir diffs · blind Opus 5.5/high verification (verify9)",
             fontsize=14, color=MUTED)
    ax = fig.add_axes([.06, .12, .43, .73], facecolor=BG)
    key = fig.add_axes([.52, .12, .46, .73])
    key.set_axis_off()
    ax.set_xscale("log")
    if cost:
        ax.set_xlim(min(p[0] for p in reference.values()) / 1.2,
                    max(p[0] for p in reference.values()) * 1.2)
        ticks = [.04, .06, .10, .15, .25, .4, .6, 1, 1.5, 2.5, 4]
        xlabel = "Estimated API cost per diff (USD, log scale)"
    else:
        ax.set_xlim(38, 320)
        ticks = [40, 50, 60, 80, 100, 120, 150, 200, 250, 300]
        xlabel = "Median review latency, slowest reviewer (seconds, log scale)"
    ax.set_ylim(0, 290)
    ax.xaxis.set_major_locator(FixedLocator(ticks))
    ax.xaxis.set_major_formatter(FuncFormatter(lambda value, _: f"{value:g}"))
    ax.xaxis.set_minor_locator(NullLocator())
    ax.set_xlabel(xlabel, labelpad=14, fontsize=13, color=INK)
    ax.set_ylabel("Confirmed bugs found across 70 diffs", labelpad=12, fontsize=13, color=INK)
    ax.spines[["top", "right"]].set_visible(False)
    for spine in ("left", "bottom"):
        ax.spines[spine].set_color("#b9b8b2")
    ax.tick_params(colors=MUTED, length=4, pad=7)
    ax.grid(axis="y", color=GRID, linewidth=.7)
    ax.set_axisbelow(True)

    marks = []
    handles = []
    for name, color, points in series:
        if cost:
            other = [(x, y) for o, (x, y, flagged) in points.items() if not flagged and o not in EXTRAS]
            ax.scatter(*zip(*other), s=18, color=color, linewidth=0, alpha=.18, zorder=1)
        frontier = sorted((x, y, o) for o, (x, y, flagged) in points.items() if flagged)
        xs, ys, _ = zip(*frontier)
        ax.step(xs, ys, where="post", color=color, linewidth=1.7, zorder=2)
        ax.scatter(xs, ys, s=55, color=color, edgecolor=BG, linewidth=1.3, zorder=3)
        marks.extend((x, y, option, color) for x, y, option in frontier)
        for option in EXTRAS:
            x, y, _ = points[option]
            ax.scatter([x], [y], s=75, marker="D", facecolor=BG,
                       edgecolor=color, linewidth=1.5, zorder=3)
            marks.append((x, y, option, color))
        handles.append(Line2D([], [], color=color, linewidth=2, marker="o",
                              markeredgecolor=BG, label=name))
    handles.append(Line2D([], [], color=MUTED, linestyle="none", marker="D", markerfacecolor=BG,
                          label="Greptile alone"))
    if cost:
        handles.append(Line2D([], [], color=MUTED, linestyle="none", marker=".", alpha=.4,
                              label="Other priced options"))
    ax.legend(handles=handles, loc="lower left", bbox_to_anchor=(-.008, 1.012),
              ncols=len(handles), frameon=False, fontsize=11, columnspacing=1.2)
    place_numbers(fig, ax, marks, number)

    key.text(.04, 1.012, "Reviewers", fontsize=14, fontweight="bold", va="bottom")
    columns = {"time": .67, "cost": .78, "All": .85, "Medium or high": .93,
               "High only": 1.0}
    for title, x in (("Time (s)", columns["time"]), ("Cost", columns["cost"]),
                     ("All", columns["All"]), ("Med+", columns["Medium or high"]),
                     ("High", columns["High only"])):
        key.text(x, 1.012, title, fontsize=10.5, color=MUTED, ha="right", va="bottom")
    step = .98 / len(order)
    finding_format = ".2f" if cost else ".0f"
    for index, option in enumerate(order):
        y = .98 - index * step
        key.text(0, y, str(number[option]), fontsize=11, fontweight="bold", va="center")
        key.text(.044, y, name_option(option), fontsize=9.5, va="center")
        all_row = key_values["All confirmed"][option]
        time_text = f"{float(all_row['median_latency_s']):.1f}"
        raw_cost = all_row["cost_per_diff_usd"]
        cost_text = f"{float(raw_cost):.3f}" if raw_cost else "—"
        key.text(columns["time"], y, time_text, fontsize=9.5, color=MUTED,
                 ha="right", va="center")
        key.text(columns["cost"], y, cost_text, fontsize=9.5, color=MUTED,
                 ha="right", va="center")
        for label, column in (("All confirmed", "All"),
                              ("Medium or high", "Medium or high"),
                              ("High only", "High only")):
            value = float(key_values[label][option]["findings"])
            color = next(color for name, color, _ in series if name == label)
            key.text(columns[column], y, format(value, finding_format), fontsize=9.5,
                     color=color, ha="right", va="center")

    if cost:
        methodology = ("Cost is estimated USD per diff; estimates cover reviewer turns and exclude carrier work, "
                       "judging and subscriptions. Only priced options appear on this chart.")
    else:
        methodology = ("Latency simulates simultaneous starts and uses the slowest reviewer. "
                       "It excludes startup and contention. Cost is estimated USD per diff; — means unknown.")
    fig.text(.05, .047, methodology, fontsize=11, color=MUTED)
    fig.text(.05, .027,
             "Findings are expected totals across 70 diffs. Greptile's repository context could not be pinned to each diff's commit.",
             fontsize=11, color=MUTED)

    output.mkdir(parents=True, exist_ok=True)
    stem = output / f"review-pareto-{measure}-findings"
    fig.savefig(stem.with_suffix(".png"), dpi=160, facecolor=BG)
    svg = StringIO()
    fig.savefig(svg, format="svg", facecolor=BG, metadata={"Date": None})
    # Strip trailing spaces from Matplotlib's wrapped SVG path commands.
    stem.with_suffix(".svg").write_text(
        "\n".join(line.rstrip() for line in svg.getvalue().splitlines()) + "\n", encoding="utf-8"
    )
    plt.close(fig)
    print(f"{stem} · {len(reference)} options including solo references · {len(order)} keyed options")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--results", type=Path, default=Path(__file__).resolve().parent)
    args = parser.parse_args()
    render(args.results, args.output, cost=False)
    render(args.results, args.output, cost=True)
