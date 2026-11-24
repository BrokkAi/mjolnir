# Sol 6.2 medium vs. Sol 6.1 medium

Replacing one Sol 6.1 medium reviewer with one Sol 6.2 medium reviewer
increased Neon-confirmed findings from **50.00 to 259.00** across the 70
historical diffs. With two Terra 6.1 high reviewers, the same replacement
increased the expected total from **138.50 to 284.33**. The new solo reviewer
also exceeded the Terra pair's 129.33 confirmed findings, with higher recorded
latency and estimated cost.

| Setup | Confirmed, all severities | Confirmed, high and medium | Median turn latency (s) | Mean total tokens per diff | Estimated API cost per diff |
|---|---:|---:|---:|---:|---:|
| Sol 6.1 medium | 50.00 | 47.50 | 46.3 | 235,047 | $0.100 |
| Sol 6.2 medium | 259.00 | 189.00 | 106.3 | 755,063 | $0.252 |
| 2 × Terra 6.1 high | 129.33 | 114.33 | 47.3 | 489,928 | $0.087 |
| Sol 6.1 medium + 2 × Terra 6.1 high | 138.50 | 122.33 | 54.3 | 724,975 | $0.187 |
| Sol 6.2 medium + 2 × Terra 6.1 high | 284.33 | 210.00 | 109.6 | 1,244,991 | $0.339 |

Confirmed findings by impact across the same 70 diffs:

| Setup | Low | Medium | High |
|---|---:|---:|---:|
| Sol 6.1 medium | 2.50 | 33.50 | 14.00 |
| Sol 6.2 medium | 70.00 | 153.00 | 36.00 |
| 2 × Terra 6.1 high | 15.00 | 91.33 | 23.00 |
| Sol 6.1 medium + 2 × Terra 6.1 high | 16.17 | 96.67 | 25.67 |
| Sol 6.2 medium + 2 × Terra 6.1 high | 74.33 | 171.00 | 39.00 |

These are the same expected union counts computed separately for each severity.
Fractional counts average reviewer assignments; displayed components can differ
from the all-severity total by 0.01 due to rounding. Most additional findings
are medium impact. High-impact findings increase from 14.00 to 36.00 solo
and from 25.67 to 39.00 with the Terra pair.

The two Terra reviewers added 25.33 confirmed findings to Sol 6.2 solo,
including 21.00 high or medium severity, for an estimated $0.087 more per
diff. All five rows use the same 70 diffs.

| Change from Sol 6.1 to 6.2 | Severity | Additional confirmed findings | 95% paired bootstrap interval | Diffs better / worse / tied |
|---|---|---:|---:|---:|
| Solo | All | +209.00 | +177.00 to +242.50 | 64 / 0 / 6 |
| Solo | High and medium | +141.50 | +114.00 to +172.00 | 58 / 0 / 12 |
| With 2 × Terra | All | +145.83 | +121.17 to +171.58 | 63 / 0 / 7 |
| With 2 × Terra | High and medium | +87.67 | +67.00 to +110.83 | 50 / 0 / 20 |

Each configuration unions groups found by its reviewers and averages the
count over all valid assignments within each diff. The old Sol arm contributed
two independent sessions per diff, averaged as alternatives for one reviewer
slot. The new arm contributed one session per diff. Both compound options
draw their two distinct Terra sessions from the same pool of three Terra
reviews on 59 diffs and four on 11. The totals sum the per-diff means. The
95% intervals use 20,000 bootstrap resamples of paired diff-level differences.
The intervals reflect variation across the sampled diffs. With one new
review per diff, the experiment cannot measure variation across repeated
6.2 reviews of the same diff.

## Coverage and adjudication

The original Stage 4 Pareto round, `verify5`, used **Opus/high**. The later
`verify6` switched to **Neon/medium** after Claude subscription profiles were
exhausted. Across 1,928 shared raw findings, the judges agreed on 88.5% of
verdicts: Neon promoted 212 plausible or rejected findings to confirmed and
demoted 10 confirmed findings to rejected. It also raised severity in 394
low-to-medium and 240 medium-to-high cases.
[Earlier judge comparison](../stage4-hydrogen-medium/consistency-vs-verify5.json)

All 6.1 and 6.2 findings in this report were judged together by Neon/medium
in `verify7`. Keep these same-round comparisons separate from the original
Opus/high frontier's absolute totals. The new 6.2 findings have only Neon's
assessment; this follow-up did not obtain Opus judgments for them.

- The 70 diffs are the same 30 historical defect candidates and 40 ordinary
  changes used for arm N. The audit matched each new review to its recorded
  intro and base, captured diff and historical context. All 70 checkouts passed
  the inventory comparison and all 70 reviewers handed back, including two
  explicit `No findings.` handbacks.
- Joint verification round `verify7` pools 221 Terra 6.1 high sessions from
  A/B, 140 Sol 6.1 medium sessions from E, and 70 Sol 6.2 medium sessions from O.
  The 11 B-arm Sol 6.1 high sessions are outside this pool. It retained
  all 431 selected sessions, including those with no findings.
- The batch builder shuffled findings within each diff and used randomized
  IDs with a private provenance map. The 34 batches covered the 68 diffs
  with findings. The other two remained in the scored corpus. The judge
  received the same spec as `verify6` and the verifier checkout at
  `3c3fa4b13`. All 34 accepted turns recorded `gemini-4-neon` with medium
  effort configured, split 17 each over `gemini` and `gemini2`. The audit
  checked finding text and provenance, commit membership, verdict enums,
  and exactly one judgment per raw finding.
- The pool contained 717 raw findings, 274 from arm O, 100 from E and 343
  from the selected Terra sessions. The judge formed 310 groups and confirmed
  291, with 2 plausible and 17 rejected. Writing style can still suggest
  provenance. The results use the judge's assessments.
- On 443 common raw findings across 61 diffs, `verify6` and `verify7`
  agreed on 98.0% of verdicts and on 84.6% of severities when both kept the
  finding. Seven previously rejected findings became confirmed and one
  previously confirmed finding became rejected. The same judge model ran
  on a narrower pool in the new round. Direct comparisons in the tables all
  use the fresh joint verdicts. [Agreement table](consistency-vs-verify6.json)

## Runtime and measurement

Arm O ran on installed Mjolnir 2.74.0, Codex 0.170.0 and Codex ACP 1.15.1.
All 70 live runtime probes matched image digest
`sha256:1f5469c1c7264c025668e465a852199bc165938f6e84e925303f4fb46ca5e0db`.
The accepted O reviewers used `codex3` on 68 diffs and `codex2` on two.
The historical cohorts ran earlier. The stage 2 record lists Codex 0.168.0 and
Codex ACP 1.15.0. Stage 3 records Mjolnir 2.71.0 at its first command.
The cohorts differed in runtime, launch conditions and profile mix, so the
observed change covers the reviewer setups as run.

The reviewer executor width was 16 after the canary. The running driver
also had its original two-permit creation and startup gate. At peak, 11
recorded reviewer turns overlapped. The gate was removed for later invocations
without restarting the active campaign. Judge concurrency was 16.

All 431 selected sessions have prompt-to-finish timing and token and cost
coverage. The latency column is the mean of sampled corpus medians. For each
diff, it uses the slowest sampled member under simultaneous starts and
excludes startup. Compound timings are simulated from separately observed
turns. The cost column uses Mjolnir's recorded API-price estimates, excludes
carrier and judge work, and does not measure subscription charges.

One canary cleanup attempt used an obsolete suspend flag. Resuming collection
with the supported command preserved the accepted review. One carrier failed
before review during daemon recovery and succeeded on a single-job retry.
The first judge launch was refused for all 34 batches because the saved
workspace no longer existed. Creating the same named workspace through the
existing helper allowed all 34 untouched batches to run. There were zero
accepted judge attempts before that recovery and zero rejected outputs
afterward. [Reproduction and recovery commands](reproduction.md)

## Saved results

- [Paired comparisons](paired-comparisons.txt)
- [Per-session timing and usage](sessions.csv), [usage summary](usage.json)
- [Finding groups](groups.csv), [session-to-group mapping](session_groups.csv)
- [Scoring coverage](score.json), [blind judgment audit](judge-audit.json),
  [judge selection audit](judge-selection.json)
- [Latency and coverage, all severities](pareto.csv),
  [latency and coverage, high and medium](pareto-sev-high-medium.csv)
