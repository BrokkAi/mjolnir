# Deliver data-derived review budget maps

This living ExecPlan follows `.agents/PLANS.md`. Keep Progress, Surprises & Discoveries, Decision Log and Outcomes & Retrospective current.

## Purpose / Big Picture

The user wants to see which reviewer combinations deliver the most confirmed findings while staying within both a time allowance and an API cost allowance. Deliver separate all-findings and medium/high PNGs, plus a self-contained interactive HTML with exact setup details on hover, to Minastirith's Downloads folder. The region boundaries must come from measured setup thresholds rather than handpicked round budgets or a chosen number of clusters.

## Progress

- [x] (2026-11-24 16:38Z) Confirmed the existing Opus result data, filtering rules, cached plotting runtime and working Minastirith transfer route.
- [x] (2026-11-24 16:38Z) Chose the complete attainable-findings surface, preserving ties and avoiding cluster or regret tolerances.
- [x] (2026-11-24 16:57:01Z) Implemented the result-local renderer and offline HTML; generated both PNG/SVG pairs and the external caption.
- [x] (2026-11-24 16:57:01Z) Independent oracle matched every observed cell and tie set; inspected both plots. Python audit: 45 ok, 0 FAIL. Chrome interaction audit: 19 ok, 0 FAIL.
- [x] (2026-11-24 16:57:01Z) Copied both PNGs, HTML and caption to Minastirith Downloads; all four remote SHA-256 values match.
- [ ] Commit, obtain a clean before-push review, merge upstream if necessary, and push.

## Surprises & Discoveries

The exported CSV coordinates are rounded. Newly computed joint efficiency must be described as conditional on those coordinates, not as an audited full-precision frontier. An option is jointly dominated when another uses no more time, costs no more, and yields at least as many findings, with at least one strict improvement. A dominated option with equal findings can still tie for the maximum once both options fit a budget; the numeric surface and tie records therefore use all eligible options.

Prior analysis found 116 priced non-Greptile setups and 196 otherwise eligible setups with unknown prices. Unknown prices are not zero. The existing two-dimensional frontier flags do not define the joint time/cost/findings surface. The main instance and raw reviewer/judge artifacts need no further work.

## Decision Log

- Decision: Compute the maximum findings under both budget constraints at every observed threshold, without clustering or a regret tolerance. Rationale: this answers the user's decision directly without inventing a weighting between seconds and dollars. Date/Author: 2026-11-24, parent.
- Decision: Retain all equal-value winners and distinguish infeasible budgets from zero findings. Rationale: resource ties and rounded counts must not become unsupported unique recommendations. Date/Author: 2026-11-24, parent.
- Decision: Use shared logarithmic resource axes and the same absolute findings color scale across the two panels; provide individual PNGs and one offline interactive HTML. Rationale: the views remain comparable and dense regions can be read through hover rather than tiny labels. Date/Author: 2026-11-24, parent.
- Decision: Exclude Greptile combinations and retain Base/Plus only as standalone reference diamonds outside optimization. Rationale: this preserves the user's chart preference and distinguishes reference performance from the optimized background. Date/Author: 2026-11-24, parent.
- Decision: Place methods and limitations in an external caption, README and surrounding HTML prose. Rationale: the image should contain readable axes, essential scope and data marks, following the visualization skill. Date/Author: 2026-11-24, parent.

## Outcomes & Retrospective

Both static maps and the interactive page are complete and delivered. The independent oracle checked all 20,930 observed cells, including 304 tied medium/high cells, with zero numeric mismatches. The implementation audit also checks rendering and real Chrome hover/pin behavior. No model calls or judgments were run. Only reviewed source publication remains.

## Context and Orientation

The repository root is the session checkout. All new implementation lives in `.agents/evals/review-arms/results/sol-62-opus-high/`, abbreviated below as the result directory. Its `pareto.csv` contains all confirmed expected findings and resource estimates for each raw setup name; `pareto-sev-high-medium.csv` contains the matching medium/high counts. Existing `plot_frontiers.py` provides display naming and Greptile filtering examples. Keep those source CSVs and the existing latency/cost charts unchanged.

A finding count is the expected union of confirmed groups over valid assignments of recorded reviewer sessions, summed across 70 historical diffs. The joint Opus 5.5/high round supplies the judgments. Time is a simulated simultaneous-start median using the slowest sampled reviewer, excluding startup and contention. Cost is estimated reviewer-turn USD per diff, excluding carrier work, judging and subscriptions. Cohorts were collected at different times. Greptile's historical repository context was not pinned to each diff's commit. These limits belong in the caption.

For a time allowance T and cost allowance C, the displayed function is the maximum findings among setups with latency <= T and cost <= C. No eligible setup means an infeasible cell, not zero findings. Each threshold is closed on its lower edge. Observed setup times and prices give the candidate boundaries; adjacent cells can be merged only when doing so preserves the displayed value and hover answers. Color must be a step function, not smoothly interpolated performance. The HTML must show the allowance separately from each winning setup's actual resource use.

The raw campaign figure directory is `/mnt/optane/review-arms-20261113/sol-62-repeats/figure/`. Its existing `render_frontiers_offline.sh` uses cached Matplotlib 3.11.2 without installation. Node 24 is available. Although no browser executable is on PATH, an existing cached headless Chrome is available at /home/jonathan/.cache/ms-playwright/chromium_headless_shell-1234/chrome-headless-shell-linux64/chrome-headless-shell. It was used without installing packages for real hover and pin checks.

## Plan of Work

The implementation child owns `plot_budget_maps.py` and its supporting result-local files, the new generated assets, external caption and README links. Its CLI is a positional output directory with optional `--results`, defaulting to the renderer's own directory. Generate `review-opus-high-budget-all.png` and `.svg`, `review-opus-high-budget-medium-high.png` and `.svg`, `review-opus-high-budget.html`, and `review-opus-high-budget-caption.txt`. Refresh the same friendly files in the raw figure directory for transfer. HTML must work offline with no external scripts, requests or storage.

The independent auditor reuses the previous breakpoint investigation but checks actual generated data against all eligible CSV rows, especially closed thresholds, ties, infeasible regions, final padded display cells and the difference between optimized background values and Greptile's own reference values. The parent alone maintains this plan, accepts the integrated result, transfers files, commits, requests review and pushes. No Rust/Go build, installation, daemon operation or model call is part of this work.

## Concrete Steps

Run the renderer from the repository root using the cached plotting environment:

    budget_result_dir=.agents/evals/review-arms/results/sol-62-opus-high
    budget_figure_dir=/mnt/optane/review-arms-20261113/sol-62-repeats/figure
    bash "$budget_figure_dir/render_frontiers_offline.sh" "$budget_result_dir/plot_budget_maps.py" "$budget_result_dir"

The renderer must produce both PNGs, both SVGs, the HTML and caption. The accepted validation commands, from the repository root, are:

    budget_chrome=/home/jonathan/.cache/ms-playwright/chromium_headless_shell-1234/chrome-headless-shell-linux64/chrome-headless-shell
    budget_audit_dir=/tmp/review-budget-audit
    mkdir -p "$budget_audit_dir"
    bash "$budget_figure_dir/render_frontiers_offline.sh" "$budget_result_dir/audit_budget_maps.py" "$budget_result_dir" --chrome "$budget_chrome" --scratch "$budget_audit_dir"
    node "$budget_result_dir/audit_budget_html.mjs" "$budget_result_dir" "$budget_audit_dir" "$budget_chrome"

The Python command reports 45 ok and 0 FAIL; the browser command reports 19 ok and 0 FAIL. Run `git diff --check`. Compare each observed threshold-cell maximum and complete tie set with an independent brute-force enumeration of all eligible CSV rows. Exercise the JavaScript lookup with the actual serialized thresholds and data, including exact boundaries and infeasible points. Inspect full images and any dense label areas with the local image viewer.

After acceptance, copy the two friendly PNGs, HTML and caption with existing SSH credentials:

    scp -F /home/jonathan/.ssh/config -o BatchMode=yes /mnt/optane/review-arms-20261113/sol-62-repeats/figure/review-opus-high-budget-all.png /mnt/optane/review-arms-20261113/sol-62-repeats/figure/review-opus-high-budget-medium-high.png /mnt/optane/review-arms-20261113/sol-62-repeats/figure/review-opus-high-budget.html /mnt/optane/review-arms-20261113/sol-62-repeats/figure/review-opus-high-budget-caption.txt jbell@minastirith:Downloads/

The established remote home is `C:\Users\jbell`; use remote PowerShell `Get-FileHash -Algorithm SHA256 -LiteralPath` on each of the four Downloads files and compare with local `sha256sum`. Do not change SSH configuration or open a remote GUI. Commit only this work's files on the current branch, call `request_review`, and push only after a clean final verdict. If upstream moved, fetch and merge `origin/master`, preserve the reviewed baseline, obtain the required merge review and retry the push.

## Validation and Acceptance

A user opening either PNG sees that panel's expected findings across 70 diffs under both resource limits, with clear units and a readable shared scale. Each image uses data-derived step boundaries. The HTML opens without a server, switches or shows both metrics, and displays every winning real setup with its actual time/cost when a region is inspected. Unknown prices never appear as free choices. Greptile combinations appear nowhere; its two solo reference diamonds expose their own findings and do not pretend to be the optimized background winner.

Actual-data checks must cover every cell, all ties, exact thresholds and infeasible values. The two source CSVs must remain byte-unchanged. Images, SVGs and HTML must agree numerically; rendering must be repeatable. The independent audit must either pass or name a precise unresolved limitation. Remote SHA-256 values must match the accepted local artifacts. Before publication, review must be clean and the final working tree must be clean after the push.

## Idempotence and Recovery

Rendering overwrites only the named generated result assets and raw friendly exports. Rerun it safely after an edit; keep source CSVs frozen. A failed transfer can be repeated after checking hashes. An interrupted review or push does not require regenerating or rejudging data. Do not clean raw campaign directories or start another verifier. This work adds no package dependency and needs no service to stop.

## Artifacts and Notes

The implementation evidence is .mj/agents/ae051a20723abc356c5f404e636b5fe5/{summary.md,audit-python.txt,audit-html.txt,shots/}. The independent evidence is .mj/agents/0f197302f2fbc342dcedefb987a1bf1d/verify/{report.md,expected.py,check_data.py,check_html.mjs,check_png.py}. Its numeric oracle and page lookup matched every cell. Its coarse PNG sampler has isolated overlay-pixel ambiguities; the exact implementation pixel audit passes, and the parent inspected the visible marks. The final pin fix changes only interaction, not data or static images.

Remote and local SHA-256 values agree: all PNG 6f93e8de6887b2c7ad92fc738096789bd371f1278d5c6c167dbdf4a8edeac7a7; medium/high PNG a6301c7055e585b2a6494a56f8c16e2beff8328a7af851fd062aede442a5d045; HTML a9da25a790a2c6ba11e465731a8fce49e6063e5fe14cfeccfd66d9954e836885; caption 43c205a8eb772fc2f2c4e64098117edf9fa455660e9cd4714a431842dc8ad963. A prior read-only report is `.mj/agents/0f197302f2fbc342dcedefb987a1bf1d/report.md`; it informs the design but does not substitute for the actual-data artifact audit.

## Interfaces and Dependencies

Use Python standard-library CSV/JSON and cached Matplotlib for rendering. Use self-contained HTML, SVG and plain JavaScript for interaction. Export the same setup identifiers and metric values to static and interactive views. The implementation may organize local functions naturally; it must have a renderer CLI as specified and a deterministic surface calculation that the independent auditor can compare with brute force. Node is a validation dependency already present, not a new runtime requirement for the user.

Revision note (2026-11-24): Created the execution plan after the user requested both maps in Downloads, recording the full-surface choice and delivery/acceptance steps before accepting implementation.

Revision note (2026-11-24 16:57:01Z): Recorded completed generation, independent numeric validation, existing-browser integration checks, cross-panel pin ownership correction and verified delivery. Source publication remains pending.
