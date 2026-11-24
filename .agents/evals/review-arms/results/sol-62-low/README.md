# Sol 6.2 low vs. Sol 6.2 and 6.1 medium

Across the original 70 diffs, one Sol 6.2 low reviewer found **155.00**
Neon-confirmed issues, compared with **50.00** for Sol 6.1 medium and
**256.00** for Sol 6.2 medium. Its solo median reviewer turn was **46.0 s**,
close to Sol 6.1 medium's 46.3 s and below Sol 6.2 medium's 106.3 s. With
two Terra 6.1 high reviewers, the corresponding totals were **197.67**,
**135.83**, and **278.67**.

| Setup | Low impact | Medium impact | High impact | Medium + high | All confirmed |
|---|---:|---:|---:|---:|---:|
| Sol 6.1 medium | 1.50 | 34.00 | 14.50 | 48.50 | 50.00 |
| Sol 6.2 medium | 55.00 | 164.00 | 37.00 | 201.00 | 256.00 |
| Sol 6.2 low | 30.00 | 101.00 | 24.00 | 125.00 | 155.00 |
| 2 × Terra 6.1 high | 13.50 | 90.67 | 22.50 | 113.17 | 126.67 |
| Sol 6.1 medium + 2 × Terra 6.1 high | 14.67 | 95.50 | 25.67 | 121.17 | 135.83 |
| Sol 6.2 medium + 2 × Terra 6.1 high | 59.50 | 180.67 | 38.50 | 219.17 | 278.67 |
| Sol 6.2 low + 2 × Terra 6.1 high | 34.50 | 134.50 | 28.67 | 163.17 | 197.67 |

Impact columns use the judge's severity labels. The evaluation counts the
union of confirmed groups for each assignment of distinct reviewers to a
setup. It then averages assignments within each diff and sums over the
70 diffs. Two old Sol sessions per diff are alternative draws for one reviewer
slot. Each new Sol effort has one session per diff. All compound setups use
the same pool of three Terra high sessions on 59 diffs and four on 11.
Totals use exact weights before rounding. [Exact counts](severity-totals-exact.json)

| Change to Sol 6.2 low | All confirmed change (95% paired interval) | Medium + high change (95% paired interval) |
|---|---:|---:|
| From 6.1 medium, solo | +105.00 (+89.50 to +121.50) | +76.50 (+61.50 to +92.50) |
| From 6.2 medium, solo | −101.00 (−125.00 to −79.00) | −76.00 (−100.00 to −54.00) |
| From 6.1 medium, with 2 × Terra | +61.83 (+47.17 to +77.50) | +42.00 (+29.08 to +56.25) |
| From 6.2 medium, with 2 × Terra | −81.00 (−101.67 to −61.67) | −56.00 (−77.17 to −37.00) |

Intervals use 20,000 bootstrap resamples of paired diff-level differences.
They measure variation across the sampled diffs. One reviewer per new
effort on each diff leaves repeat-to-repeat variation unmeasured.
The intervals condition on the saved outputs and Neon judgments and do not
quantify judge uncertainty. For high-impact findings alone, replacing 6.1
medium with 6.2 low alongside the Terra pair adds 3.00, with a 95% interval
of −1.00 to +7.83.
The saved comparisons also give intervals for each impact tier.
[Paired comparisons](paired-comparisons.txt)

## Latency and usage

| Setup | Median latency (s) | Mean total tokens per diff | Estimated API cost per diff |
|---|---:|---:|---:|
| Sol 6.1 medium | 46.3 | 235,047 | $0.100 |
| Sol 6.2 medium | 106.3 | 755,063 | $0.252 |
| Sol 6.2 low | 46.0 | 290,193 | $0.120 |
| 2 × Terra 6.1 high | 47.3 | 489,928 | $0.087 |
| Sol 6.1 medium + 2 × Terra 6.1 high | 54.3 | 724,975 | $0.187 |
| Sol 6.2 medium + 2 × Terra 6.1 high | 109.5 | 1,244,991 | $0.339 |
| Sol 6.2 low + 2 × Terra 6.1 high | 52.1 | 780,121 | $0.207 |

All 501 reviewer sessions have prompt-to-finish timing, tokens, and recorded
estimated API cost. Latency is the mean of sampled corpus medians, with each
diff taking its slowest sampled reviewer under simultaneous starts. Compound
latency is simulated and excludes session startup. Estimated costs exclude
carrier and judge work and do not measure subscription charges.
[Pareto data](pareto.csv) · [Usage detail](usage.json)

Arms O and P used Mjolnir 2.74.0, Codex 0.170.0, Codex ACP 1.15.1 and image
`sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db`.
Arm P used `codex3` on 67 diffs and `codex2` on three, compared with 68
and two in O. The cohorts ran separately. The O driver had a two-permit
creation and startup gate and recorded 11 overlapping reviewer turns at peak.
Arm P used executor width 16 after the canary with that gate removed and
recorded 12 overlapping reviewer turns at peak. The older 6.1 cohorts used
earlier runtimes. Timing and usage therefore describe the recorded setups.

## Coverage and verification

- Arm P adds one Sol 6.2 low handback on each original diff. All 70 passed model,
  effort, review input, inventory, image, and prompt-clock checks. Three
  handbacks explicitly reported no findings. The canary counts toward the 70.
  All 70 succeeded without retries.
- Round `verify8` pooled 501 reviewers from A/B/E/O/P, including 221 Terra high,
  140 Sol 6.1 medium, 70 Sol 6.2 medium and 70 Sol 6.2 low. The 881 raw findings
  include 164 from P. The two diffs with no findings remain in the analysis.
- All 34 judge batches ran with concurrency 16 and no retries, split equally
  over `gemini` and `gemini2`. The saved judge selections record
  `gemini-4-neon` with medium effort configured. The verifier used checkout
  `3c3fa4b13` and the same specification as `verify7`. Batches shuffled raw
  findings and removed provenance fields. Finding text can still suggest
  provenance. The audit checked each finding's input text, commit membership
  and exactly one valid judgment. Judges grouped findings into 333 distinct
  groups, with 305 confirmed, 2 plausible and 26 rejected.
- The primary tables use only joint `verify8` judgments. For the 717 raw
  findings judged in both rounds, `verify7` and `verify8` agreed on 97.9% of
  verdicts and 90.0% of retained severity labels. Checksums confirmed the old
  verification rounds stayed intact. [Agreement table](consistency-vs-verify7.json)

[Reproduction commands](reproduction.md) · [Scoring coverage](score.json) ·
[Judge audit](judge-audit.json) · [Judge selection audit](judge-selection.json)

[Per-session measurements](sessions.csv) · [Judged groups](groups.csv) ·
[Session-to-group mapping](session_groups.csv)
