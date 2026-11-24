"""Render budget maps: best expected findings within a latency AND cost budget.

F(T, C) is the maximum findings among priced, non-Greptile options whose
median latency is <= T and whose estimated cost per diff is <= C. F is a step
function; it only changes at option latencies and costs, so the map uses those
observed values as its cell edges (closed lower bound, no interpolation).
Writes two PNG/SVG maps, one self-contained interactive HTML and a caption.
"""
import argparse
import csv
import hashlib
import html
import json
import math
import re
from io import StringIO
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt
from matplotlib.collections import LineCollection, PolyCollection
from matplotlib.colors import Normalize, to_hex
from matplotlib.ticker import FixedLocator, FuncFormatter, NullLocator

from plot_frontiers import BG, INK, MUTED, name_option

PNG_DPI = 160
STEM = "review-opus-high-budget"
PANELS = [
    # id, short label, CSV, plain description
    ("all", "All confirmed", "pareto.csv", "all confirmed findings"),
    ("medium-high", "Medium + high impact", "pareto-sev-high-medium.csv",
     "medium- and high-impact confirmed findings"),
]
GREPTILE_SOLO = ("Greptile/base", "Greptile/plus")
TIME_TICKS = [40, 50, 60, 80, 100, 150, 200, 250]
COST_TICKS = [.05, .1, .2, .5, 1, 2, 3]
PAD = 1.12  # display padding around the observed extremes on the log axes
CMAP = matplotlib.colormaps["viridis"]
RULE = ("F(T,C) = max findings among priced non-Greptile options with "
        "latency <= T and cost <= C; cell (i,j) = [ts[i],ts[i+1]) x [cs[j],cs[j+1]); "
        "blank = no option fits")


# ---------------------------------------------------------------- data
def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def load_panel(results, filename):
    with (results / filename).open(newline="") as source:
        rows = list(csv.DictReader(source))
    options, greptile, unpriced_names = [], [], []
    unpriced = greptile_rows = 0
    for row in rows:
        name = row["option"]
        if "Greptile/" in name:
            greptile_rows += 1
            if name in GREPTILE_SOLO:
                greptile.append(entry(row))
            continue
        if not row["cost_per_diff_usd"]:
            unpriced += 1  # unknown price is not free: omitted from optimisation
            unpriced_names.append(name)
            continue
        options.append(entry(row))
    greptile.sort(key=lambda g: GREPTILE_SOLO.index(g["name"]))
    return {"options": options, "greptile": greptile, "unpriced": unpriced,
            "unpriced_names": unpriced_names,
            "greptile_rows": greptile_rows, "rows": len(rows)}


def reviewer_tokens(names):
    """Distinct 'model/effort' reviewer labels in setup names such as '2× Terra/high + Sol/medium'."""
    return {name_option(re.sub(r"^\d+×\s*", "", part))
            for name in names for part in name.split(" + ")}


def model_summary(options, unpriced_names):
    efforts = {}
    for label in sorted(reviewer_tokens(o["name"] for o in options)):
        family, effort = label.split("/")
        efforts.setdefault(family, []).append(effort)
    rank = ["low", "medium", "high", "xhigh"]
    priced = [f"{family} ({', '.join(sorted(e, key=rank.index))})" for family, e in efforts.items()]
    priced_labels = reviewer_tokens(o["name"] for o in options)
    families = sorted({label.split("/")[0] for label in reviewer_tokens(unpriced_names) - priced_labels})
    return {"priced": priced, "unpriced_families": families,
            "reviewers": [min(o["n"] for o in options), max(o["n"] for o in options)]}


def entry(row):
    return {"name": row["option"], "label": name_option(row["option"]),
            "t": float(row["median_latency_s"]), "c": float(row["cost_per_diff_usd"]),
            "f": float(row["findings"]), "n": int(row["reviewers"])}


def winners(options, t, c):
    """Every option tied for the maximum findings within both budgets."""
    feasible = [o for o in options if o["t"] <= t and o["c"] <= c]
    if not feasible:
        return None, []
    best = max(o["f"] for o in feasible)
    return best, [o for o in feasible if o["f"] == best]


def surface(options):
    ts = sorted({o["t"] for o in options})
    cs = sorted({o["c"] for o in options})
    grid = [[winners(options, t, c)[0] for c in cs] for t in ts]
    return ts, cs, grid


def reduce_surface(ts, cs, grid):
    """Drop a threshold only when its row/column equals the previous one (score unchanged)."""
    keep_t = [i for i in range(len(ts)) if i == 0 or grid[i] != grid[i - 1]]
    rows = [grid[i] for i in keep_t]
    keep_c = [j for j in range(len(cs))
              if j == 0 or any(row[j] != row[j - 1] for row in rows)]
    return ([ts[i] for i in keep_t], [cs[j] for j in keep_c],
            [[row[j] for j in keep_c] for row in rows])


def dominated_tie(member, tied):
    return any(p is not member and p["t"] <= member["t"] and p["c"] <= member["c"]
               and (p["t"] < member["t"] or p["c"] < member["c"]) for p in tied)


def ever_winners(options, ts, cs):
    """Options that are in the tied argmax set of at least one threshold cell."""
    seen, tie_only = set(), set()
    for t in ts:
        for c in cs:
            _, tied = winners(options, t, c)
            for o in tied:
                seen.add(o["name"])
                if dominated_tie(o, tied):
                    tie_only.add(o["name"])
    by_name = {o["name"]: o for o in options}
    order = sorted(seen, key=lambda n: (by_name[n]["t"], by_name[n]["c"], n))
    return order, sorted(tie_only)


def price_break(options):
    """Vertex of the cost staircase's concave hull where cost per finding jumps most."""
    steps, best = [], -1
    for o in sorted(options, key=lambda o: (o["c"], -o["f"], o["t"], o["name"])):
        if o["f"] > best:
            steps.append(o)
            best = o["f"]
    hull = []
    for p in steps:
        while len(hull) >= 2 and ((hull[-1]["c"] - hull[-2]["c"]) * (p["f"] - hull[-2]["f"])
                                  - (hull[-1]["f"] - hull[-2]["f"]) * (p["c"] - hull[-2]["c"])) >= 0:
            hull.pop()
        hull.append(p)
    slopes = [(b["c"] - a["c"]) / (b["f"] - a["f"]) for a, b in zip(hull, hull[1:])]
    jumps = [(slopes[k] / slopes[k - 1], hull[k]) for k in range(1, len(slopes))]
    return max(jumps, key=lambda j: j[0])[1] if jumps else None


def anchors(options):
    fastest = min(options, key=lambda o: (o["t"], o["c"], o["name"]))
    cheapest = min(options, key=lambda o: (o["c"], o["t"], o["name"]))
    best = max(options, key=lambda o: (o["f"], -o["c"], -o["t"], o["name"]))
    roles = {}
    for role, option in (("Fastest" if fastest is not cheapest else "Fastest and cheapest", fastest),
                         ("Cheapest", cheapest if cheapest is not fastest else None),
                         ("Price break", price_break(options)),
                         ("Most findings", best)):
        if option:
            roles.setdefault(option["name"], []).append(role)
    return [{"name": n, "roles": r} for n, r in roles.items()]


# ---------------------------------------------------------------- geometry
def extended(ts, cs, grid, domain):
    """Edges and a (nc+1)x(nt+1) level matrix whose first row/column is infeasible (None)."""
    xs = [domain["t"][0], *ts, domain["t"][1]]
    ys = [domain["c"][0], *cs, domain["c"][1]]
    z = [[None] * (len(xs) - 1) for _ in range(len(ys) - 1)]
    for i in range(len(ts)):
        for j in range(len(cs)):
            z[j + 1][i + 1] = grid[i][j]
    return xs, ys, z


def boundaries(xs, ys, z):
    """Merged segments where adjacent cells differ in score or feasibility."""
    segments = []
    for i in range(len(xs) - 2):  # vertical edges at xs[i+1]
        start = None
        for j in range(len(ys)):
            differs = j < len(ys) - 1 and z[j][i] != z[j][i + 1]
            if differs and start is None:
                start = j
            if not differs and start is not None:
                segments.append(((xs[i + 1], ys[start]), (xs[i + 1], ys[j])))
                start = None
    for j in range(len(ys) - 2):  # horizontal edges at ys[j+1]
        start = None
        for i in range(len(xs)):
            differs = i < len(xs) - 1 and z[j][i] != z[j + 1][i]
            if differs and start is None:
                start = i
            if not differs and start is not None:
                segments.append(((xs[start], ys[j + 1]), (xs[i], ys[j + 1])))
                start = None
    return segments


def build_panel(results, panel_id, short, filename, description, domain):
    data = load_panel(results, filename)
    options = data["options"]
    ts, cs, grid = surface(options)
    rts, rcs, rgrid = reduce_surface(ts, cs, grid)
    order, tie_only = ever_winners(options, ts, cs)
    return {
        "id": panel_id, "title": short, "description": description, "csv": filename,
        "options": options, "greptile": data["greptile"],
        "full_thresholds": [len(ts), len(cs)],
        "ts": rts, "cs": rcs, "f": rgrid,
        "max_f": max(o["f"] for o in options),
        "ever_winners": order, "tie_only_winners": tie_only,
        "anchors": anchors(options),
        "models": model_summary(options, data["unpriced_names"]),
        "counts": {"priced": len(options), "unpriced_omitted": data["unpriced"],
                   "greptile_rows_omitted": data["greptile_rows"] - len(data["greptile"]),
                   "greptile_solo": len(data["greptile"]), "rows": data["rows"]},
    }


# ---------------------------------------------------------------- PNG / SVG
def wrap_label(label, width=30):
    """Break a long setup name before a '+', so it stays beside its dot."""
    if len(label) <= width:
        return label
    parts, lines = label.split(" + "), []
    for part in parts:
        if lines and len(lines[-1]) + len(part) + 3 <= width:
            lines[-1] += " + " + part
        else:
            lines.append(("+ " if lines else "") + part)
    return "\n".join(lines)


def draw(panel, domain, vmax, geometry):
    norm = Normalize(0, vmax)
    xs, ys, z = extended(panel["ts"], panel["cs"], panel["f"], domain)
    plt.rcParams.update({"font.family": "DejaVu Sans", "font.size": 12,
                         "svg.fonttype": "none", "svg.hashsalt": "review-arms-budget-map"})
    fig = plt.figure(figsize=(12, 9.4), facecolor=BG, dpi=PNG_DPI)
    ax = fig.add_axes([.085, .185, .70, .665], facecolor=BG)
    cax = fig.add_axes([.84, .185, .026, .665])
    fig.text(.045, .955, "Best review findings within a latency and cost budget",
             fontsize=21, fontweight="bold", color=INK)
    fig.text(.045, .918,
             f"{panel['description'].capitalize()} · expected total across 70 historical diffs · "
             f"{panel['counts']['priced']} priced setups",
             fontsize=13, color=MUTED)

    # One rectangle per horizontal run of equal score. A same-colour hairline edge hides the
    # anti-aliasing seams that adjacent quads show in browsers.
    runs = []
    for j in range(len(ys) - 1):
        i = 0
        while i < len(xs) - 1:
            if z[j][i] is None:
                i += 1
                continue
            k = i
            while k + 1 < len(xs) - 1 and z[j][k + 1] == z[j][i]:
                k += 1
            runs.append((z[j][i], [(xs[i], ys[j]), (xs[k + 1], ys[j]), (xs[k + 1], ys[j + 1]), (xs[i], ys[j + 1])]))
            i = k + 1
    colors = [CMAP(norm(v)) for v, _ in runs]
    ax.add_collection(PolyCollection([r for _, r in runs], facecolors=colors, edgecolors=colors,
                                     linewidths=.4, zorder=1))
    ax.add_collection(LineCollection(boundaries(xs, ys, z), colors="white", linewidths=.7,
                                     alpha=.85, zorder=2))
    ax.set_xscale("log")
    ax.set_yscale("log")
    ax.set_xlim(*domain["t"])
    ax.set_ylim(*domain["c"])
    for axis, ticks, fmt in ((ax.xaxis, TIME_TICKS, "{:g}"), (ax.yaxis, COST_TICKS, "${:g}")):
        axis.set_major_locator(FixedLocator([t for t in ticks]))
        axis.set_major_formatter(FuncFormatter(lambda v, _, fmt=fmt: fmt.format(v)))
        axis.set_minor_locator(NullLocator())
    ax.set_xlabel("Latency budget: slowest reviewer's median (seconds, log scale)",
                  fontsize=13, color=INK, labelpad=10)
    ax.set_ylabel("Cost budget: estimated API cost per diff (USD, log scale)",
                  fontsize=13, color=INK, labelpad=10)
    ax.tick_params(colors=MUTED, length=4, pad=6, labelsize=12)
    for spine in ax.spines.values():
        spine.set_color("#b9b8b2")

    by_name = {o["name"]: o for o in panel["options"]}
    tie_only = set(panel["tie_only_winners"])
    ax.scatter([by_name[n]["t"] for n in panel["ever_winners"] if n not in tie_only],
               [by_name[n]["c"] for n in panel["ever_winners"] if n not in tie_only],
               s=26, facecolor="white", edgecolor=INK, linewidth=.9, zorder=4)
    if tie_only:
        ax.scatter([by_name[n]["t"] for n in tie_only], [by_name[n]["c"] for n in tie_only],
                   s=26, facecolor="none", edgecolor="white", linewidth=1, zorder=4)
    labels = []
    for g in panel["greptile"]:
        ax.scatter([g["t"]], [g["c"]], s=170, marker="D", facecolor=CMAP(norm(g["f"])),
                   edgecolor="white", linewidth=2.2, zorder=5)
        ax.scatter([g["t"]], [g["c"]], s=170, marker="D", facecolor="none",
                   edgecolor=INK, linewidth=1, zorder=6)
        labels.append(ax.annotate(f"{g['label']} alone: {g['f']:.2f} findings\n{g['t']:.1f} s · ${g['c']:.2f}",
                    (g["t"], g["c"]), xytext=(-13, -2), textcoords="offset points",
                    ha="right", va="center", fontsize=10.5, color=INK, zorder=7,
                    bbox={"facecolor": BG, "edgecolor": "none", "alpha": .88, "pad": 2.2}))

    for item in panel["anchors"]:
        o = by_name[item["name"]]
        role = " and ".join(item["roles"])
        # Place each label toward the larger empty side of the map.
        dx = 24 if o["t"] < 150 else -24
        dy = 22 if role.startswith(("Fastest", "Cheapest")) else -34
        labels.append(ax.annotate(f"{role}\n{wrap_label(o['label'])}: {o['f']:.2f}\n{o['t']:.1f} s · ${o['c']:.3f}",
                    (o["t"], o["c"]), xytext=(dx, dy), textcoords="offset points",
                    ha="left" if dx > 0 else "right", va="bottom" if dy > 0 else "top",
                    fontsize=10.5, color=INK, zorder=8,
                    bbox={"facecolor": BG, "edgecolor": "none", "alpha": .9, "pad": 2.2},
                    arrowprops={"arrowstyle": "-", "color": INK, "linewidth": .8,
                                "shrinkA": 0, "shrinkB": 5}))

    gradient = [[v] for v in [vmax * k / 255 for k in range(256)]]
    cax.imshow(gradient, cmap=CMAP, norm=norm, origin="lower", aspect="auto",
               extent=(0, 1, 0, vmax))
    cax.set_xticks([])
    cax.yaxis.tick_right()
    cax.yaxis.set_label_position("right")
    ticks = [*range(0, int(vmax) + 1, 50), panel["max_f"]]
    cax.set_yticks(ticks)
    cax.set_yticklabels([f"{v:g}" for v in ticks[:-1]] + [f"{panel['max_f']:.2f}"])
    cax.get_yticklabels()[-1].set_fontweight("bold")
    cax.get_yticklabels()[-1].set_color(INK)
    cax.tick_params(colors=MUTED, length=4, labelsize=12)
    cax.set_ylabel("Expected confirmed findings (best setup within both limits)",
                   fontsize=12, color=INK, labelpad=12)
    for spine in cax.spines.values():
        spine.set_color("#b9b8b2")
    cax.plot([0, 1], [panel["max_f"]] * 2, color="white", linewidth=1.6)

    fig.text(.045, .045, "Blank: infeasible     \u25cb best setup     \u25c7 Greptile solo reference"
             "     Bold tick: panel best", fontsize=11, color=MUTED)

    fig.canvas.draw()
    box = ax.get_window_extent()
    height = fig.get_size_inches()[1] * fig.dpi
    geometry[panel["id"]] = {
        "width": int(round(fig.get_size_inches()[0] * fig.dpi)), "height": int(round(height)),
        "axes": [box.x0, height - box.y1, box.x1, height - box.y0],
        # Label boxes and leader lines cover the map; pixel audits skip them.
        "occluders": [[b.x0, height - b.y1, b.x1, height - b.y0]
                      for a in labels for b in (patch.get_window_extent()
                      for patch in (a.get_bbox_patch(), a.arrow_patch) if patch is not None)],
    }
    return fig


# ---------------------------------------------------------------- HTML
def svg_panel(panel, domain, vmax):
    """Static SVG for one panel in a 640x600 viewBox; JS adds hover only."""
    norm = Normalize(0, vmax)
    x0, y0, w, h = 66, 14, 556, 500
    lx0, lx1 = math.log(domain["t"][0]), math.log(domain["t"][1])
    ly0, ly1 = math.log(domain["c"][0]), math.log(domain["c"][1])

    def px(t):
        return x0 + w * (math.log(t) - lx0) / (lx1 - lx0)

    def py(c):
        return y0 + h * (1 - (math.log(c) - ly0) / (ly1 - ly0))

    xs, ys, z = extended(panel["ts"], panel["cs"], panel["f"], domain)
    out = [f'<svg class="map" id="map-{panel["id"]}" viewBox="0 0 640 600" role="img" '
           f'aria-label="{html.escape(panel["title"])} budget map" '
           f'data-x0="{x0}" data-y0="{y0}" data-w="{w}" data-h="{h}">']
    out.append(f'<rect x="{x0}" y="{y0}" width="{w}" height="{h}" fill="#fcfcfb"/>')
    out.append('<g class="cells" shape-rendering="crispEdges">')
    for j in range(len(ys) - 1):
        i = 0
        while i < len(xs) - 1:
            v = z[j][i]
            if v is None:
                i += 1
                continue
            k = i
            while k + 1 < len(xs) - 1 and z[j][k + 1] == v:
                k += 1
            color = to_hex(CMAP(norm(v)))
            out.append(f'<rect x="{px(xs[i]):.2f}" y="{py(ys[j + 1]):.2f}" '
                       f'width="{px(xs[k + 1]) - px(xs[i]):.2f}" '
                       f'height="{py(ys[j]) - py(ys[j + 1]):.2f}" fill="{color}" '
                       f'stroke="{color}" stroke-width=".5" data-f="{v}"/>')
            i = k + 1
    out.append("</g>")
    path = "".join(f"M{px(a[0]):.2f} {py(a[1]):.2f}L{px(b[0]):.2f} {py(b[1]):.2f}"
                   for a, b in boundaries(xs, ys, z))
    out.append(f'<path d="{path}" fill="none" stroke="#fff" stroke-width=".8" '
               f'stroke-opacity=".85" pointer-events="none"/>')
    for ticks, domain_key, pos, horizontal in ((TIME_TICKS, "t", px, True),
                                               (COST_TICKS, "c", py, False)):
        for tick in ticks:
            if not domain[domain_key][0] < tick < domain[domain_key][1]:
                continue
            p = pos(tick)
            if horizontal:
                out.append(f'<line x1="{p:.2f}" x2="{p:.2f}" y1="{y0 + h}" y2="{y0 + h + 5}" class="tick"/>'
                           f'<text x="{p:.2f}" y="{y0 + h + 21}" text-anchor="middle" class="tl">{tick:g}</text>')
            else:
                out.append(f'<line x1="{x0 - 5}" x2="{x0}" y1="{p:.2f}" y2="{p:.2f}" class="tick"/>'
                           f'<text x="{x0 - 9}" y="{p + 4:.2f}" text-anchor="end" class="tl">${tick:g}</text>')
    out.append(f'<rect x="{x0}" y="{y0}" width="{w}" height="{h}" fill="none" stroke="#b9b8b2"/>')
    out.append(f'<text x="{x0 + w / 2}" y="{y0 + h + 46}" text-anchor="middle" class="al">'
               "Latency budget: slowest reviewer's median (s, log)</text>")
    out.append(f'<text transform="translate(16 {y0 + h / 2}) rotate(-90)" text-anchor="middle" '
               'class="al">Cost budget: estimated API USD per diff (log)</text>')
    by_name = {o["name"]: o for o in panel["options"]}
    tie_only = set(panel["tie_only_winners"])
    out.append('<g class="markers">')
    for index, name in enumerate(panel["ever_winners"]):
        o = by_name[name]
        style = 'fill="none" stroke="#fff"' if name in tie_only else 'fill="#fff" stroke="#0b0b0b"'
        out.append(f'<circle cx="{px(o["t"]):.2f}" cy="{py(o["c"]):.2f}" r="3.6" {style} '
                   f'stroke-width="1" data-option="{html.escape(name)}"/>')
    for index, g in enumerate(panel["greptile"]):
        gx, gy = px(g["t"]), py(g["c"])
        color = to_hex(CMAP(norm(g["f"])))
        out.append(f'<path d="M{gx:.2f} {gy - 9:.2f}L{gx + 9:.2f} {gy:.2f}L{gx:.2f} {gy + 9:.2f}'
                   f'L{gx - 9:.2f} {gy:.2f}Z" fill="{color}" stroke="#fff" stroke-width="3" '
                   f'paint-order="stroke" data-greptile="{index}"/>'
                   f'<path d="M{gx:.2f} {gy - 9:.2f}L{gx + 9:.2f} {gy:.2f}L{gx:.2f} {gy + 9:.2f}'
                   f'L{gx - 9:.2f} {gy:.2f}Z" fill="none" stroke="#0b0b0b" stroke-width="1" '
                   'pointer-events="none"/>')
    out.append("</g>")
    out.append('<g class="cross" pointer-events="none"></g></svg>')
    return "\n".join(out)


def colorbar_svg(vmax):
    stops = "".join(f'<stop offset="{k / 10:.1f}" stop-color="{to_hex(CMAP(k / 10))}"/>'
                    for k in range(11))
    ticks = "".join(f'<line x1="{20 + 400 * v / vmax:.1f}" x2="{20 + 400 * v / vmax:.1f}" y1="22" y2="28" '
                    f'stroke="#52514e"/><text x="{20 + 400 * v / vmax:.1f}" y="42" text-anchor="middle" '
                    f'class="tl">{v}</text>' for v in range(0, int(vmax) + 1, 50))
    return (f'<svg viewBox="0 0 440 66" class="bar" role="img" aria-label="Color scale, 0 to '
            f'{vmax:.1f} expected findings"><defs><linearGradient id="g">{stops}</linearGradient></defs>'
            f'<rect x="20" y="6" width="400" height="16" fill="url(#g)"/>{ticks}'
            '<text x="220" y="60" text-anchor="middle" class="al">Expected confirmed findings '
            "(best setup within both limits)</text></svg>")


CSS = """
:root{color-scheme:light}body{margin:0;background:#fcfcfb;color:#0b0b0b;
font:15px/1.45 -apple-system,"Segoe UI",Roboto,"DejaVu Sans",sans-serif}
main{max-width:1320px;margin:0 auto;padding:24px 20px 48px}
h1{font-size:26px;margin:0 0 4px}h2{font-size:18px;margin:0 0 4px}
.sub{color:#52514e;margin:0 0 14px}.panels{display:grid;gap:20px;
grid-template-columns:repeat(auto-fit,minmax(520px,1fr))}
figure{margin:0}svg.map{width:100%;height:auto;display:block;cursor:crosshair}
svg text{font-family:inherit;fill:#52514e}.tl{font-size:12px}.al{font-size:13px;fill:#0b0b0b}
.tick{stroke:#52514e}.bar{width:min(440px,100%);display:block;margin:8px 0}
#tip{position:fixed;display:none;pointer-events:none;background:#fff;border:1px solid #b9b8b2;
border-radius:4px;padding:8px 10px;max-width:380px;font-size:13px;line-height:1.35;
box-shadow:0 2px 8px rgba(0,0,0,.18);z-index:9}
#tip b{font-weight:600}#tip .m{color:#52514e}#tip ul{margin:4px 0 0;padding-left:16px}
.legend{color:#52514e;font-size:13px;margin:4px 0 16px}
details{margin:12px 0}summary{cursor:pointer;font-weight:600}
table{border-collapse:collapse;font-size:13px;margin-top:8px}td,th{padding:2px 10px 2px 0;text-align:left}
td.n{text-align:right;font-variant-numeric:tabular-nums}.notes li{margin:4px 0;color:#262522}
"""

JS = r"""
const D = JSON.parse(document.getElementById('budget-data').textContent);
const tip = document.getElementById('tip');
let pinned = null;  // the one pinned map, or null; owned globally so no map can overwrite another's pin
const crosses = [];
function clearAll() { tip.style.display = 'none'; for (const c of crosses) c.replaceChildren(); }
const fmtT = v => v.toFixed(1) + ' s', fmtC = v => '$' + v.toFixed(3), fmtF = v => v.toFixed(2);
function tied(P, T, C) {
  const feasible = P.options.filter(o => o.t <= T && o.c <= C);
  if (!feasible.length) return null;
  const best = Math.max(...feasible.map(o => o.f));
  const win = feasible.filter(o => o.f === best).map(o => ({o, dominated: false}));
  for (const w of win) w.dominated = win.some(p => p !== w && p.o.t <= w.o.t && p.o.c <= w.o.c &&
    (p.o.t < w.o.t || p.o.c < w.o.c));
  win.sort((a, b) => a.dominated - b.dominated || a.o.c - b.o.c || a.o.t - b.o.t || (a.o.name < b.o.name ? -1 : 1));
  return {best, win};
}
function upper(a, v) { let lo = 0, hi = a.length; while (lo < hi) { const m = (lo + hi) >> 1; if (a[m] <= v) lo = m + 1; else hi = m; } return lo - 1; }
function gridScore(P, T, C) { const i = upper(P.ts, T), j = upper(P.cs, C); return i < 0 || j < 0 ? null : P.f[i][j]; }
function cellRange(P, T, C) {
  const i = upper(P.ts, T), j = upper(P.cs, C);
  const t1 = i + 1 < P.ts.length ? fmtT(P.ts[i + 1]) : null, c1 = j + 1 < P.cs.length ? fmtC(P.cs[j + 1]) : null;
  return `latency ${fmtT(P.ts[i])} to ${t1 ? 'below ' + t1 : 'and above'}, cost ${fmtC(P.cs[j])} to ${c1 ? 'below ' + c1 : 'and above'}`;
}
function el(tag, cls, text) { const e = document.createElement(tag); if (cls) e.className = cls; if (text !== undefined) e.textContent = text; return e; }
function optionItem(o, note) {
  const li = el('li'); li.append(el('b', '', o.label), document.createTextNode(` — ${fmtT(o.t)}, ${fmtC(o.c)}, ${fmtF(o.f)} findings` + (note ? ` (${note})` : ''))); return li;
}
function budgetTip(P, T, C) {
  const box = el('div'); const r = tied(P, T, C);
  box.append(el('div', 'm', `${P.title}: budget ≤ ${fmtT(T)} and ≤ ${fmtC(C)}`));
  if (!r) { box.append(el('div', '', 'No priced setup fits both limits.')); return box; }
  const check = gridScore(P, T, C);
  if (check !== r.best) box.append(el('div', '', `DATA MISMATCH: map ${check}, options ${r.best}`));
  box.append(el('div', '', `Best: ${fmtF(r.best)} expected findings across 70 diffs`));
  box.append(el('div', 'm', `Same score for ${cellRange(P, T, C)}`));
  const ul = el('ul');
  for (const w of r.win) ul.append(optionItem(w.o, w.dominated ? 'tied, but another tied setup is no slower and no costlier' : ''));
  box.append(el('div', 'm', r.win.length > 1 ? `${r.win.length} setups tie for the best score:` : 'Winning setup:'), ul);
  return box;
}
function markerTip(P, kind, index) {
  const box = el('div');
  if (kind === 'g') {
    const g = P.greptile[index], r = tied(P, g.t, g.c);
    box.append(el('div', '', `${g.label} alone (reference, not optimized)`));
    box.append(el('div', '', `Actual: ${fmtF(g.f)} findings, ${fmtT(g.t)}, ${fmtC(g.c)}`));
    box.append(el('div', 'm', r ? `Best priced non-Greptile setup within these limits: ${fmtF(r.best)} findings` : 'No priced setup fits these limits'));
    return box;
  }
  const o = P.options.find(x => x.name === index);
  box.append(el('div', '', `${o.label}: ${fmtF(o.f)} findings, ${fmtT(o.t)}, ${fmtC(o.c)}`));
  box.append(el('div', 'm', 'This setup is best at some budget; the map is coloured at its own corner by the best setup within these limits.'));
  box.append(budgetTip(P, o.t, o.c));
  return box;
}
function place(ev) {
  const w = tip.offsetWidth, h = tip.offsetHeight;
  tip.style.left = Math.max(4, Math.min(innerWidth - w - 4, ev.clientX + 14)) + 'px';
  tip.style.top = Math.max(4, Math.min(innerHeight - h - 4, ev.clientY + 14)) + 'px';
}
function setup(P) {
  const svg = document.getElementById('map-' + P.id);
  const g = k => +svg.dataset[k];
  const [x0, y0, w, h] = [g('x0'), g('y0'), g('w'), g('h')];
  const [t0, t1] = D.domain.t.map(Math.log), [c0, c1] = D.domain.c.map(Math.log);
  const px = t => x0 + w * (Math.log(t) - t0) / (t1 - t0), py = c => y0 + h * (1 - (Math.log(c) - c0) / (c1 - c0));
  const cross = svg.querySelector('.cross');
  crosses.push(cross);
  function show(ev) {
    const pt = svg.createSVGPoint(); pt.x = ev.clientX; pt.y = ev.clientY;
    const p = pt.matrixTransform(svg.getScreenCTM().inverse());
    if (p.x < x0 || p.x > x0 + w || p.y < y0 || p.y > y0 + h) { clearAll(); return false; }
    const T = Math.exp(t0 + (p.x - x0) / w * (t1 - t0)), C = Math.exp(c0 + (1 - (p.y - y0) / h) * (c1 - c0));
    let best = null, bestD = 10;
    const consider = (kind, id, t, c) => { const d = Math.hypot(px(t) - p.x, py(c) - p.y); if (d < bestD) { bestD = d; best = [kind, id]; } };
    P.greptile.forEach((q, i) => consider('g', i, q.t, q.c));
    for (const n of P.ever_winners) { const o = P.options.find(x => x.name === n); consider('o', n, o.t, o.c); }
    clearAll();
    tip.replaceChildren(best ? markerTip(P, best[0], best[1]) : budgetTip(P, T, C));
    tip.style.display = 'block'; place(ev);
    const ns = 'http://www.w3.org/2000/svg';
    for (const [a, b, c, d] of [[x0, p.y, x0 + w, p.y], [p.x, y0, p.x, y0 + h]]) {
      const l = document.createElementNS(ns, 'line');
      l.setAttribute('x1', a); l.setAttribute('y1', b); l.setAttribute('x2', c); l.setAttribute('y2', d);
      l.setAttribute('stroke', '#0b0b0b'); l.setAttribute('stroke-width', '.7'); l.setAttribute('stroke-dasharray', '3 3');
      cross.append(l);
    }
    return true;
  }
  svg.addEventListener('pointermove', ev => { if (!pinned) show(ev); });
  svg.addEventListener('pointerleave', () => { if (!pinned) clearAll(); });
  // A click pins this map (switching from any other map), or unpins when this map is already pinned.
  svg.addEventListener('click', ev => {
    if (pinned === svg) { pinned = null; show(ev); }
    else if (show(ev)) pinned = svg;
  });
}
document.addEventListener('keydown', ev => { if (ev.key === 'Escape') { pinned = null; clearAll(); } });
for (const P of D.panels) setup(P);
"""


def winners_table(panel):
    by_name = {o["name"]: o for o in panel["options"]}
    rows = "".join(
        f'<tr><td>{html.escape(by_name[n]["label"])}</td><td class="n">{by_name[n]["t"]:.1f}</td>'
        f'<td class="n">{by_name[n]["c"]:.3f}</td><td class="n">{by_name[n]["f"]:.2f}</td></tr>'
        for n in panel["ever_winners"])
    return (f'<details><summary>{html.escape(panel["title"])}: {len(panel["ever_winners"])} setups '
            'that are best at some budget</summary><table><tr><th>Setup</th><th>Latency (s)</th>'
            f'<th>Cost (USD)</th><th>Findings</th></tr>{rows}</table></details>')


def notes(payload):
    first = payload["panels"][0]["counts"]
    m = payload["panels"][0]["models"]
    return [
        "Each map colors every latency and cost budget by the best expected findings among setups "
        "that fit both limits. Region edges are the observed setups' own latencies and costs; "
        "nothing is interpolated or binned.",
        f"{first['priced']} priced setups are optimized. {first['unpriced_omitted']} setups with "
        "an unknown price are omitted, because an unknown price is not a free one. "
        f"{first['greptile_rows_omitted']} Greptile combinations are omitted. "
        "Greptile alone appears as two diamonds at its own measured findings, latency and cost; "
        "its repository context could not be pinned to each diff's commit.",
        "Both panels share one absolute color scale, so the medium + high panel stays in the lower "
        "half of it. Compare colors across panels, not region shapes.",
        "Cost covers reviewer turns only. It excludes carrier work, judging and subscriptions. "
        "Latency simulates simultaneous starts and takes the slowest reviewer's median; it "
        "excludes startup and contention.",
        "Findings are expected confirmed totals across 70 historical diffs from current "
        "Opus 5.5/high judgments. Inputs are the published CSVs' rounded values, so this is not an "
        "audited full-precision frontier. Equal rounded scores count as ties.",
        f"Priced setups combine {m['reviewers'][0]} to {m['reviewers'][1]} parallel reviewers from "
        f"{', '.join(m['priced'])}. Setups with {' or '.join(m['unpriced_families'])} reviewers "
        "have no price and are omitted.",
    ]


def build_html(payload, vmax):
    panels = "\n".join(
        f'<figure><h2>{html.escape(p["title"])}</h2><p class="sub">{html.escape(p["description"].capitalize())}</p>{svg_panel(p, payload["domain"], vmax)}</figure>'
        for p in payload["panels"])
    data = json.dumps(payload, separators=(",", ":"), ensure_ascii=True).replace("</", "<\\/")
    items = "".join(f"<li>{html.escape(n)}</li>" for n in notes(payload))
    tables = "".join(winners_table(p) for p in payload["panels"])
    return f"""<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Best review findings within a latency and cost budget</title><style>{CSS}</style></head>
<body><main>
<h1>Best review findings within a latency and cost budget</h1>
<p class="sub">Expected confirmed findings across 70 historical diffs. Hover a map for the winning setups,
their latency and cost; click to pin, Esc to release.</p>
{colorbar_svg(vmax)}
<div class="panels">
{panels}
</div>
<p class="legend">Blank: no priced setup fits both limits. White lines: where the best score changes.
White dots: setups that are best at some budget. Diamonds: Greptile alone at its own findings,
latency and cost; the background is the optimization without Greptile.
The last row and column extend beyond the largest observed value.</p>
<ul class="notes">{items}</ul>
{tables}
<div id="tip" role="status"></div>
<script id="budget-data" type="application/json">{data}</script>
<script id="app">{JS}</script>
</main></body></html>
"""


# ---------------------------------------------------------------- caption
def caption(payload, vmax):
    p_all, p_mh = payload["panels"]
    lines = ["Budget maps: best expected review findings within a latency and cost budget", ""]
    for item in notes(payload):
        lines.append("- " + item)
    lines.append(f"- Both maps use the color range 0 to {vmax:.1f}. The all-findings panel peaks at "
                 f"{p_all['max_f']:.2f}; the medium + high panel peaks at {p_mh['max_f']:.2f}.")
    lines.append("- Extent: latency "
                 f"{payload['domain']['t'][0]:.1f} to {payload['domain']['t'][1]:.1f} s and cost "
                 f"${payload['domain']['c'][0]:.3f} to ${payload['domain']['c'][1]:.3f}, "
                 "chosen to include every option and both Greptile diamonds; the last observed "
                 "row and column are open-ended.")
    lines.append("- Price break: the vertex of the cost-versus-findings staircase's concave hull "
                 "where cost per added finding jumps most. It describes this option set only.")
    lines.append("- The existing frontier flags in the CSVs do not define membership here. A setup "
                 "is marked when it is in the tied best set of at least one budget cell.")
    for p in payload["panels"]:
        by_name = {o["name"]: o for o in p["options"]}
        tied = len(p["tie_only_winners"])
        lines.append(f"- {p['title']}: {len(p['ever_winners'])} setups are best at some budget "
                     f"({tied} only by tying a setup that is no slower and no costlier); "
                     f"{p['full_thresholds'][0]} x {p['full_thresholds'][1]} observed thresholds "
                     f"reduce to {len(p['ts'])} x {len(p['cs'])} score-changing ones.")
    return "\n".join(lines) + "\n"


# ---------------------------------------------------------------- driver
def insert_svg_metadata(svg, panel_payload):
    body = json.dumps(panel_payload, separators=(",", ":"), ensure_ascii=True)
    marker = svg.index(">", svg.index("<svg")) + 1
    return (svg[:marker] + f'\n<metadata id="budget-map-data"><![CDATA[{body}]]></metadata>'
            + svg[marker:])


def render(results, output):
    sources = {name: sha256(results / name) for _, _, name, _ in PANELS}
    probe = [load_panel(results, name) for _, _, name, _ in PANELS]
    core = lambda d: [(o["name"], o["t"], o["c"]) for o in d["options"]]
    if core(probe[0]) != core(probe[1]) or [g["t"] for g in probe[0]["greptile"]] != \
            [g["t"] for g in probe[1]["greptile"]]:
        raise ValueError("The two CSVs list different priced options")
    points = probe[0]["options"] + probe[0]["greptile"]
    domain = {"t": [min(o["t"] for o in probe[0]["options"]) / PAD, max(o["t"] for o in points) * PAD],
              "c": [min(o["c"] for o in probe[0]["options"]) / PAD, max(o["c"] for o in points) * PAD]}
    panels = [build_panel(results, *spec, domain) for spec in PANELS]
    vmax = max(p["max_f"] for p in panels)

    output.mkdir(parents=True, exist_ok=True)
    geometry, svgs = {}, {}
    for panel in panels:
        fig = draw(panel, domain, vmax, geometry)
        fig.savefig(output / f"{STEM}-{panel['id']}.png", dpi=PNG_DPI, facecolor=BG)
        buffer = StringIO()
        fig.savefig(buffer, format="svg", facecolor=BG, metadata={"Date": None})
        svgs[panel["id"]] = "\n".join(line.rstrip() for line in buffer.getvalue().splitlines()) + "\n"
        plt.close(fig)
    for panel in panels:
        panel["png"] = geometry[panel["id"]]
    payload = {"schema": 1, "rule": RULE, "sources": sources, "domain": domain,
               "vmax": vmax, "colormap": "viridis", "panels": panels}
    for panel in panels:
        text = insert_svg_metadata(svgs[panel["id"]], {"schema": 1, "domain": domain,
                                                       "vmax": vmax, "panel": panel})
        (output / f"{STEM}-{panel['id']}.svg").write_text(text, encoding="utf-8")
    (output / f"{STEM}.html").write_text(build_html(payload, vmax), encoding="utf-8")
    (output / f"{STEM}-caption.txt").write_text(caption(payload, vmax), encoding="utf-8")
    (output / f"{STEM}-data.json").write_text(
        json.dumps(payload, separators=(",", ":"), ensure_ascii=True) + "\n", encoding="utf-8")
    for panel in panels:
        print(f"{panel['id']}: {len(panel['options'])} options, "
              f"{panel['full_thresholds']} thresholds -> {len(panel['ts'])} x {len(panel['cs'])}, "
              f"{len(panel['ever_winners'])} winners ({len(panel['tie_only_winners'])} tie-only)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("--results", type=Path, default=Path(__file__).resolve().parent)
    args = parser.parse_args()
    render(args.results, args.output)
