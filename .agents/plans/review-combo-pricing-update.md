# Refresh combination charts with the newly verified prices

This ExecPlan follows `.agents/PLANS.md`. Keep Progress, discoveries, decisions and outcomes current. The parent performs this task personally as requested by the user; no new model reviews or judges are required.

## Purpose / Big Picture

Update the latency/cost combination frontiers and the two latency-and-cost budget maps using the latest verify10 Opus judgments, including the single Sol 6.2/high sample. Add the same Hydrogen, Kimi and Ember API-equivalent prices used in the solo chart. Copy the refreshed PNGs to Minastirith Downloads. Keep Greptile solo-only, DSF/medium unplotted, friendly Sol labels and both Time/Cost key columns.

## Progress

- [x] Located the archived verify9 renderers, latest verify10 tables and the verified pricing overlay.
- [x] Reconstructed all 571 options' exact confirmed group unions and added costs by linear expectation; checked all rounded yields against the original tables.
- [x] Render and inspect the two frontier charts and two budget maps with explicit Hydrogen/Ember cost assumptions.
- [x] Audited exact cost membership, 34/30 frontier-key rows, 72/1,164 plotted marks, surfaces and image pixels; targeted browser pin reruns passed. Four PNGs copied to Downloads with matching remote hashes.
- [x] (2026-11-25 03:40Z) Publication ae2e842 passed review by recorded scope overrides; upstream-only merge 8b7f0ce passed clean review and was pushed to origin/master.

## Surprises & Discoveries

The old cost charts omitted 196 combinations containing unpriced models. The latest cohort has 571 combinations; 386 exclude Greptile entirely, two are Greptile solos, and 183 are excluded Greptile combinations. Full-precision cost frontier membership must be recalculated after pricing. All findings and latency observations already exist; no paid model work is needed. Terra has three or four retained samples per diff, so averaging its session costs globally would weight some diffs too heavily.

The reused pixel audit excluded all candidate coordinates, including hundreds of invisible dominated setups. It now skips only the markers and reduced boundaries actually rendered, preserving its original minimum sample coverage. The click audit compared fractional pointer moves with integer-rounded Chrome clicks; both now use the same physical pixel. The browser audit waits for a render frame after input, rather than using sleeps.

## Decision Log

Use a new publication directory `.agents/evals/review-arms/results/sol-62-high/combo-pricing/` to preserve the archived verify9 charts and the frozen verify10 scoring tables. Reuse copies of the existing renderers and actual-data audits. Cost is the sum of each selected configuration's mean within each diff and then across the 70 diffs; expectation is linear even when repeated reviewers are sampled without replacement. Reconstruct exact fractional findings from the scored groups/session links and compute the cost frontier on those exact counts and costs. Preserve the existing audited latency flags and observations. Greptile is excluded from frontier competition and budget optimization, but its two solos remain references.

Hydrogen's saved unspecified usage scope remains an explicit assumption. Ember's plotted price is the conservative upper context-tier bound, and key costs show the lower/upper range for setups containing Ember. These assumptions apply to every combination containing those models. Budget maps use the same upper prices. They use published rounded findings and latency thresholds, not an invented full-precision latency surface.

The review proposed dropping all measured Terra (and, in one finding, Sol 6.1) controls. Those findings were declined: the earlier "terra/historical" instruction was interpreted as the separately plotted historical Terra reference, already omitted from the solo chart. The established measured 70-diff control cohort remains in these combination updates. This records the interpretation used, not a new user instruction.

## Outcomes & Retrospective

All four PNGs are rendered and delivered. Exact frontier/SVG/key checks passed. The budget surfaces and browser threshold/oracle checks passed; targeted pixel and pin reruns resolved audit assumptions exposed by the expanded candidate set. Image labels are positioned without overlap. Failed-run and passing-rerun logs are retained with the delivery receipt. The exact repricing pass finds 17 all-impact, 20 medium+high and 16 high-only cost-frontier options before merging the three plot keys.

## Context and Orientation

The original source CSVs are in `.agents/evals/review-arms/results/sol-62-high/`. They represent 1,796 retained reviewer sessions on 70 diffs, freshly adjudicated in 35 blinded Opus5.5/high batches. Raw scored tables remain under `/mnt/optane/review-arms-20261113/verify10`. `additional-model-prices.json` beside the source CSVs supplies the verified API rates and four per-configuration costs (two Hydrogen efforts, Kimi/high, Ember/xhigh). The archived chart/audit source is under `results/sol-62-opus-high`.

## Plan of Work

`combo-pricing/build_combo_prices.py` parses existing one-to-three-reviewer setup labels, reconstructs exact expected group unions and writes new CSVs with full-precision mean costs, bounds and exact cost frontier flags. It verifies the old counts and already priced totals, and writes provenance to `combo-pricing.json`. Update the copied frontier renderer to show verify10, the cost bounds and scope assumptions, and align plot/key tops. Update the budget renderer's explanations and audit counts to derive from the new inputs, preserving the already audited pointer and pin behavior. The historical narrow-cell regression remains tested against a fresh brute-force oracle.

## Concrete Steps

From the repository root, run:

    python3 -B .agents/evals/review-arms/results/sol-62-high/combo-pricing/build_combo_prices.py

For rendering and numeric/image auditing, use the existing offline wrapper at `/mnt/optane/review-arms-20261113/sol-62-repeats/figure/render_frontiers_offline.sh`, which supplies cached Matplotlib dependencies without installation. From the new publication directory, invoke it with `plot_frontiers.py .`, then `plot_budget_maps.py .`, then `audit_budget_maps.py . --chrome /home/jonathan/.cache/ms-playwright/chromium_headless_shell-1234/chrome-headless-shell-linux64/chrome-headless-shell --scratch /tmp/review-combo-pricing-audit`. Run `node audit_budget_html.mjs . /tmp/review-combo-pricing-browser /home/jonathan/.cache/ms-playwright/chromium_headless_shell-1234/chrome-headless-shell-linux64/chrome-headless-shell`. Logs and delivery receipts belong under `/mnt/optane/review-arms-20261113/sol-62-high/combo-pricing/`.

## Validation and Acceptance

All 571 exact expected yields must match each original published severity total within its rounding tolerance. Existing priced costs must match the original three-decimal values within rounding. The 386 non-Greptile setups must have prices; Greptile combinations must not appear in plot clouds, keys or budget options. Check exact cost-frontier dominance independently and verify rendered key values/flags against the new CSVs. Budget-map threshold cells, edge probes, colors and embedded data must pass the reused brute-force audit. Browser hover/pin and narrow-view behavior must pass the existing interaction suite on the new data. Visually inspect every PNG. Copy only PNGs to Minastirith Downloads, compare remote SHA-256 to local, then complete required commit/review/push.

## Idempotence and Recovery

The repricing script rewrites derived artifacts only; original scored CSVs and all reviewer/judge inputs remain unchanged. Rerendering is deterministic. Diagnose any failed validation before copying. Repeat a failed transfer using the same validated PNG and verify its remote checksum. No daemon, profile, model collection, builds or subscriptions are changed.

## Interfaces and Dependencies

Use Python's standard library for group/count/cost reconstruction, cached Matplotlib for charts, and the installed headless Chrome and Node for the existing browser audits. The cost range columns are derived data; neither endpoint is a statistical confidence interval.
