"""Audit the budget-map artifacts against the CSVs by brute force.

Usage: audit_budget_maps.py ARTIFACT_DIR [--results DIR]

Reads the CSVs afresh (no renderer code), recomputes F(T,C) for every threshold
cell and for off-grid probes, and compares the result with the embedded data,
the HTML rectangles and sampled PNG pixels. Exits non-zero on any failure.
"""
import argparse
import bisect
import csv
import json
import math
import random
import re
import subprocess
import sys
from pathlib import Path

import matplotlib
import matplotlib.image as mpimg
from matplotlib.colors import Normalize

STEM = "review-opus-high-budget"
PANELS = {"all": "budget-pareto.csv", "medium-high": "budget-pareto-sev-high-medium.csv"}
failures = []


def check(ok, message):
    print(("ok   " if ok else "FAIL ") + message)
    if not ok:
        failures.append(message)


def raw_options(results, filename):
    rows = list(csv.DictReader((results / filename).open(newline="")))
    priced = [r for r in rows if r["cost_per_diff_usd"] and "Greptile" not in r["option"]]
    return rows, [(r["option"], float(r["median_latency_s"]), float(r["cost_per_diff_usd"]),
                   float(r["findings"])) for r in priced]


def brute(options, t, c):
    """(best, sorted tied names) by direct scan; None when nothing fits."""
    best, names = None, []
    for name, lt, lc, f in options:
        if lt <= t and lc <= c:
            if best is None or f > best:
                best, names = f, [name]
            elif f == best:
                names.append(name)
    return best, sorted(names)


def lookup(panel, t, c):
    i = bisect.bisect_right(panel["ts"], t) - 1
    j = bisect.bisect_right(panel["cs"], c) - 1
    return None if i < 0 or j < 0 else panel["f"][i][j]


def audit_surface(panel, rows, options):
    pid = panel["id"]
    names = sorted(o[0] for o in options)
    check(sorted(o["name"] for o in panel["options"]) == names, f"{pid}: embedded options equal the 386 priced non-Greptile CSV rows")
    check(len(options) == 386 and len(rows) - len(options) - sum("Greptile" in r["option"] for r in rows) == 0,
          f"{pid}: 386 priced and 0 unpriced non-Greptile rows")
    embedded = {o["name"]: (o["t"], o["c"], o["f"]) for o in panel["options"]}
    check(all(embedded[n] == (t, c, f) for n, t, c, f in options), f"{pid}: option latency, cost and findings match the CSV exactly")
    ts = sorted({o[1] for o in options})
    cs = sorted({o[2] for o in options})
    check([len(ts), len(cs)] == panel["full_thresholds"], f"{pid}: {len(ts)} x {len(cs)} thresholds")
    bad = ties = infeasible = 0
    full = [[brute(options, t, c) for c in cs] for t in ts]
    for i, t in enumerate(ts):
        for j, c in enumerate(cs):
            best, tied = full[i][j]
            if best is None:
                infeasible += 1
            ties += len(tied) > 1
            if lookup(panel, t, c) != best:
                bad += 1
    check(bad == 0, f"{pid}: all {len(ts) * len(cs)} threshold cells equal brute force (ties {ties}, infeasible {infeasible})")
    # Monotone in both budgets.
    mono = all(full[i][j][0] >= full[i - 1][j][0] for i in range(1, len(ts)) for j in range(len(cs))) and \
        all(full[i][j][0] >= full[i][j - 1][0] for i in range(len(ts)) for j in range(1, len(cs)))
    check(mono, f"{pid}: F is non-decreasing in latency and in cost")
    # Every kept threshold changes the score; every dropped one was redundant (brute-force on full grid).
    rows_kept = [i for i in range(len(ts)) if ts[i] in set(panel["ts"])]
    check(sorted(panel["ts"]) == [ts[i] for i in range(len(ts)) if i == 0 or [x[0] for x in full[i]] != [x[0] for x in full[i - 1]]],
          f"{pid}: kept latency edges are exactly those where some column changes")
    keep_cs = [cs[j] for j in range(len(cs)) if j == 0 or any(full[i][j][0] != full[i][j - 1][0] for i in range(len(ts)))]
    check(sorted(panel["cs"]) == keep_cs, f"{pid}: kept cost edges are exactly those where some row changes")
    # Off-grid probes: exact edges, one ulp below each edge, random log-uniform points incl. outside.
    domain = panel_domain
    rng = random.Random(7)
    probes = []
    for t in ts:
        for c in cs[::7] + [cs[-1]]:
            probes += [(t, c), (math.nextafter(t, 0), c), (t, math.nextafter(c, 0))]
    for c in cs:
        for t in ts[::7] + [ts[-1]]:
            probes += [(t, c), (math.nextafter(t, 0), c), (t, math.nextafter(c, 0))]
    for _ in range(40000):
        probes.append((math.exp(rng.uniform(math.log(domain["t"][0] * .8), math.log(domain["t"][1] * 1.2))),
                       math.exp(rng.uniform(math.log(domain["c"][0] * .8), math.log(domain["c"][1] * 1.2)))))
    wrong = [(t, c) for t, c in probes if lookup(panel, t, c) != brute(options, t, c)[0]]
    check(not wrong, f"{pid}: {len(probes)} edge/one-ulp-below/random probes equal brute force")
    # Argmax tie sets from the embedded options equal the CSV scan; flag resource-dominated tie members.
    dominated_ties = 0
    for t in ts[::3]:
        for c in cs[::3]:
            best, tied = brute(options, t, c)
            by = {n: (lt, lc) for n, lt, lc, _ in options}
            for n in tied:
                dominated_ties += any(m != n and by[m][0] <= by[n][0] and by[m][1] <= by[n][1]
                                      and by[m] != by[n] for m in tied)
    print(f"info {pid}: resource-dominated tie members at sampled cells: {dominated_ties}; "
          f"ever-winners {len(panel['ever_winners'])}, tie-only {len(panel['tie_only_winners'])}")
    return ts, cs, full


def audit_greptile(panel, rows):
    for g in panel["greptile"]:
        row = next(r for r in rows if r["option"] == g["name"])
        check((g["t"], g["c"], g["f"]) == (float(row["median_latency_s"]), float(row["cost_per_diff_usd"]), float(row["findings"])),
              f"{panel['id']}: {g['name']} reference equals its CSV row")
    check(all(g["name"] not in {o["name"] for o in panel["options"]} for g in panel["greptile"]),
          f"{panel['id']}: Greptile is outside the optimized options")


def audit_html(html, payload):
    scripts = re.findall(r"<script id=\"([^\"]+)\"[^>]*>(.*?)</script>", html, re.S)
    data = json.loads(dict(scripts)["budget-data"].replace("<\\/", "</"))
    check(data == payload, "html: embedded JSON equals the data file")
    check(not re.search(r"(https?:)?//[a-z0-9.-]+\.[a-z]{2,}|<link|@import|src=|href=|fetch\(|XMLHttpRequest|localStorage|sessionStorage",
                        html.replace("http://www.w3.org/2000/svg", "")), "html: no external reference or storage use")
    js = dict(scripts)["app"]
    done = subprocess.run(["node", "--check", "--input-type=commonjs"], input=js, capture_output=True, text=True)
    check(done.returncode == 0, "html: node --check passes on the embedded script " + done.stderr.strip()[:200])
    check(html.count("<svg class=\"map\"") == 2, "html: two map panels")
    return data


def audit_html_rects(html, payload, panel, options):
    pid = panel["id"]
    svg = re.search(rf'<svg class="map" id="map-{pid}".*?</svg>', html, re.S).group(0)
    attrs = {k: float(v) for k, v in re.findall(r'data-(x0|y0|w|h)="([\d.]+)"', svg)}
    rects = [tuple(float(m) for m in r[:4]) + (float(r[4]),) for r in re.findall(
        r'<rect x="([\d.]+)" y="([\d.]+)" width="([\d.]+)" height="([\d.]+)" fill="#\w+" stroke="#\w+" stroke-width="\.5" data-f="([\d.]+)"/>', svg)]
    dom = payload["domain"]
    lt0, lt1, lc0, lc1 = (math.log(v) for v in (*dom["t"], *dom["c"]))
    x0, y0, w, h = attrs["x0"], attrs["y0"], attrs["w"], attrs["h"]
    ts = sorted({o[1] for o in options})
    cs = sorted({o[2] for o in options})
    xedge = [x0 + (math.log(t) - lt0) / (lt1 - lt0) * w for t in ts]
    yedge = [y0 + h * (1 - (math.log(c) - lc0) / (lc1 - lc0)) for c in cs]
    rng = random.Random(11)
    miss = checked = 0
    for _ in range(30000):
        px, py = rng.uniform(x0, x0 + w), rng.uniform(y0, y0 + h)
        # Coordinates carry 2 decimals; stay clear of cell edges by 3x that rounding.
        if min(abs(px - x) for x in xedge) < .03 or min(abs(py - y) for y in yedge) < .03 \
                or abs(px - x0) < .03 or abs(py - (y0 + h)) < .03:
            continue
        t = math.exp(lt0 + (px - x0) / w * (lt1 - lt0))
        c = math.exp(lc0 + (1 - (py - y0) / h) * (lc1 - lc0))
        expect = brute(options, t, c)[0]
        got = {r[4] for r in rects if r[0] <= px <= r[0] + r[2] and r[1] <= py <= r[1] + r[3]}
        checked += 1
        miss += got != (set() if expect is None else {expect})
    check(miss == 0, f"html {pid}: {len(rects)} rects, {checked} random points equal brute force (blank where infeasible)")


def pixel_audit(kind, img, panel, options):
    """Compare sampled pixels of a PNG (scale 1) or a browser screenshot of the SVG with brute force."""
    pid = panel["id"]
    geo = panel["png"]
    scale = img.shape[1] / geo["width"]
    check(abs(img.shape[0] - geo["height"] * scale) < 1.5, f"{kind} {pid}: {img.shape[1]} x {img.shape[0]} matches metadata at scale {scale:.4f}")
    ax0, ay0, ax1, ay1 = (v * scale for v in geo["axes"])
    occluders = [[v * scale for v in box] for box in geo["occluders"]]
    # A browser lays out SVG text with its own font metrics, so label boxes need a wider margin.
    margin = 3 if kind == "png" else 24
    dom = payload_global["domain"]
    lt0, lt1, lc0, lc1 = (math.log(v) for v in (*dom["t"], *dom["c"]))
    norm = Normalize(0, payload_global["vmax"])
    cmap = matplotlib.colormaps["viridis"]
    ts = panel["ts"]
    cs = panel["cs"]
    xedge = [ax0 + (math.log(t) - lt0) / (lt1 - lt0) * (ax1 - ax0) for t in ts]
    yedge = [ay1 - (math.log(c) - lc0) / (lc1 - lc0) * (ay1 - ay0) for c in cs]
    markers = [(ax0 + (math.log(o["t"]) - lt0) / (lt1 - lt0) * (ax1 - ax0),
                ay1 - (math.log(o["c"]) - lc0) / (lc1 - lc0) * (ay1 - ay0), 12 * scale) for o in panel["options"] if o["name"] in panel["ever_winners"]]
    markers += [(ax0 + (math.log(g["t"]) - lt0) / (lt1 - lt0) * (ax1 - ax0),
                 ay1 - (math.log(g["c"]) - lc0) / (lc1 - lc0) * (ay1 - ay0), 28 * scale) for g in panel["greptile"]]
    rng = random.Random(3)
    tested = bad = blank_bad = blank_seen = 0
    for _ in range(60000):
        px, py = rng.uniform(ax0 + 4, ax1 - 4), rng.uniform(ay0 + 4, ay1 - 4)
        if any(b[0] - margin <= px <= b[2] + margin and b[1] - margin <= py <= b[3] + margin for b in occluders):
            continue
        if any(math.hypot(px - mx, py - my) < r for mx, my, r in markers):
            continue
        if min(abs(px - x) for x in xedge) < 3.5 or min(abs(py - y) for y in yedge) < 3.5 \
                or abs(px - ax0) < 3.5 or abs(py - ay1) < 3.5:
            continue
        t = math.exp(lt0 + (px - ax0) / (ax1 - ax0) * (lt1 - lt0))
        c = math.exp(lc0 + (1 - (py - ay0) / (ay1 - ay0)) * (lc1 - lc0))
        best = brute(options, t, c)[0]
        pixel = img[int(py), int(px), :3]
        tested += 1
        if best is None:
            blank_seen += 1
            blank_bad += max(abs(pixel[k] - (252, 252, 251)[k] / 255) for k in range(3)) > 2.5 / 255
        else:
            expected = cmap(norm(best))[:3]
            bad += max(abs(pixel[k] - expected[k]) for k in range(3)) > 2.5 / 255
    check(tested > 15000 and blank_seen > 500 and bad == 0 and blank_bad == 0,
          f"{kind} {pid}: {tested} sampled pixels equal brute-force colour ({blank_seen} infeasible pixels all blank; "
          f"colour mismatches {bad}, non-blank infeasible {blank_bad})")


def audit_svg(art, panel, payload):
    text = (art / f"{STEM}-{panel['id']}.svg").read_text()
    body = re.search(r'<metadata id="budget-map-data"><!\[CDATA\[(.*?)\]\]></metadata>', text, re.S).group(1)
    data = json.loads(body)
    check(data["panel"] == panel and data["domain"] == payload["domain"] and data["vmax"] == payload["vmax"],
          f"svg {panel['id']}: metadata equals the data file")
    check("http://" not in text.replace("http://www.w3.org", "").replace("http://purl.org", "")
          .replace("http://creativecommons.org", "").replace("http://www.w3.org/1999/xlink", ""),
          f"svg {panel['id']}: no external reference")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("artifacts", type=Path)
    parser.add_argument("--results", type=Path, default=Path(__file__).resolve().parent)
    parser.add_argument("--chrome", help="chrome-headless-shell binary, to rasterize the SVGs")
    parser.add_argument("--scratch", default=".", help="directory for browser screenshots")
    args = parser.parse_args()
    art = args.artifacts
    payload = json.loads((art / f"{STEM}-data.json").read_text())
    payload_global = payload
    panel_domain = payload["domain"]
    html = (art / f"{STEM}.html").read_text()
    audit_html(html, payload)
    import hashlib
    for name, digest in payload["sources"].items():
        check(hashlib.sha256((args.results / name).read_bytes()).hexdigest() == digest, f"source hash {name}")
    for panel in payload["panels"]:
        rows, options = raw_options(args.results, PANELS[panel["id"]])
        audit_surface(panel, rows, options)
        audit_greptile(panel, rows)
        audit_html_rects(html, payload, panel, options)
        pixel_audit("png", mpimg.imread(art / f"{STEM}-{panel['id']}.png"), panel, options)
        audit_svg(art, panel, payload)
        if args.chrome:
            Path(args.scratch).mkdir(parents=True, exist_ok=True)
            shot = Path(args.scratch) / f"svg-{panel['id']}.png"
            width = round(12 * 96)
            subprocess.run([args.chrome, "--no-sandbox", "--disable-gpu", f"--screenshot={shot}",
                            f"--window-size={width},{round(9.4 * 96) + 1}",
                            "--force-device-scale-factor=1", (art / f"{STEM}-{panel['id']}.svg").resolve().as_uri()],
                           check=True, capture_output=True)
            pixel_audit("svg-in-browser", mpimg.imread(shot), panel, options)
    check(payload["panels"][0]["max_f"] == 265.0 and payload["panels"][1]["max_f"] == 140.0, "verify10 panel maxima 265 and 140")
    sys.exit(1 if failures else 0)
