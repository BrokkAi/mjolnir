# Bifrost-dev strict-construction oracle arm (OS, run as OX): readout

Registration: `brief/PLAN-bifrost-oracle-strict.md`, with two amendments. The first covers codex4 and the reviewer draw. The second voids round 1 and labels exclusions by the gate that failed. Data root: `/mnt/optane/review-arms-bifrost-20261116`. Construction records are in this directory.

## Result: Inconclusive by retention. 10 of 17 complete facts could not be stated without leaking.

Only 1 of the 17 bifrost-dev relation diffs produced a brief that passed every gate. Each such brief has to state the complete decisive fact with no element dropped or added and no inference needed. Both an Opus and a Sol leak judge must read it as `missed` at high confidence, and a blind cause check on each rationale must clearly answer no. The registered reading needs at least 8 retained diffs, so the outcome is **Inconclusive** whatever any reviewer would score.

**Deviation: reviewers and round N skipped.** With 1 diff retained, the registered reading is Inconclusive regardless of scores, so running OX and AF2 on that one diff would add no information. The supervisor agreed, and no OX or AF2 reviewer and no round N judging were run. (The plan calls the arm "OS"; it runs as `OX` because `OS` already names stage A's Sol oracle arm.)

## Exclusions by gate

| Gate in the final round | Diffs | What it means |
|---|---:|---|
| Retained | 1 | dependency-failure (round 2) |
| Leak | 10 | the complete fact could not be stated, within three rounds, without the Opus/Sol pair flagging the defect |
| Fidelity (editor failure) | 6 | the editor dropped or added an element, or broke the brief rules. This says nothing about whether the fact can be stated without leaking |
| Cause | 0 | the one brief that reached the cause check passed |

Notes:
- **python-module-mutation** was read as `missed` by both judges in rounds 2 and 3, but Opus gave only medium confidence each time. That fails the high-confidence leak rule, and is an unresolved judgment rather than an affirmative leak. Its round 4 edit then failed fidelity by adding facts, so it is excluded as an editor failure.
- **incompatible-semantic-bundle-late-gate**'s final-round exclusion came from the brief length lint, 7,017 characters against the 7,000 limit (17 over), not from a dropped element.
- Round 1 is void. Its editor instructions did not state the no-additions rule, and all 17 round 1 edits failed fidelity, mostly on added facts.

## Per-diff history

Each round shows fidelity, then the two leak labels with confidences; "cause no → pass" marks the cause check.

| Diff | Outcome | Round 1 (void) | Round 2 | Round 3 | Round 4 |
|---|---|---|---|---|---|
| python-filescope-enclosing-tie | fidelity (editor failure) | fidelity fail (inferred) | Opus partial/low, Sol partial/high | Opus partial/medium, Sol partial/medium | fidelity fail (inferred) |
| cpp-template-inherited-base-signature | leak | fidelity fail (inferred) | Opus partial/low, Sol caught/high | Opus partial/low, Sol partial/medium | Opus caught/medium, Sol caught/high |
| incompatible-semantic-bundle-late-gate | fidelity (editor failure) | fidelity fail | Opus partial/low, Sol partial/medium | Opus partial/medium, Sol partial/medium | lint fail |
| retained-fill-java-memo-loses-gap | leak | fidelity fail | Opus partial/low, Sol partial/high | Opus partial/low, Sol partial/medium | Opus caught/medium, Sol caught/high |
| kotlin-source-constructor-steals-class-header-comment | fidelity (editor failure) | fidelity fail | fidelity fail | Opus partial/low, Sol partial/medium | fidelity fail |
| mcp-default-advisory-reported-clean | leak | fidelity fail | fidelity fail | Opus partial/low, Sol caught/medium | Opus caught/medium, Sol caught/high |
| go-receiver-poisons-package-result-contract-discovery | leak | fidelity fail (inferred) | Opus partial/low, Sol caught/medium | fidelity fail (inferred) | Opus caught/medium, Sol caught/high |
| python-module-binding-representative-after-event | leak | fidelity fail | Opus partial/low, Sol caught/high | fidelity fail | Opus caught/high, Sol caught/high |
| jvm-protected-unindexed-hierarchy-denied | leak | fidelity fail | Opus partial/low, Sol partial/medium | Opus partial/medium, Sol partial/medium | Opus caught/medium, Sol caught/medium |
| python-module-mutation-repeated-scan-budget | fidelity (editor failure) | fidelity fail | Opus missed/medium, Sol missed/high | Opus missed/medium, Sol missed/high | fidelity fail |
| dependency-failure-drops-independent-model-evidence | retained (round 2) | fidelity fail | Opus missed/high, Sol missed/high; cause no → pass | — | — |
| php-indexed-write-reenters-current-rhs | fidelity (editor failure) | fidelity fail (inferred) | Opus partial/low, Sol caught/medium | fidelity fail (inferred) | fidelity fail |
| kotlin-value-class-this-loses-underlying-projection | leak | fidelity fail | Opus partial/low, Sol partial/high | Opus partial/low, Sol partial/medium | Opus partial/low, Sol partial/medium |
| go-nilness-ignores-modeled-nonreturn | leak | fidelity fail (inferred) | fidelity fail (inferred) | fidelity fail | Opus partial/low, Sol partial/medium |
| java-static-getter-drops-incomplete-route | leak | fidelity fail | Opus partial/low, Sol partial/medium | Opus partial/low, Sol partial/medium | Opus caught/medium, Sol partial/medium |
| unproven-override-family-certifies-empty | fidelity (editor failure) | fidelity fail | Opus partial/low, Sol caught/high | fidelity fail | fidelity fail |
| js-synthetic-owner-leaks-file-summary | leak | fidelity fail | Opus partial/low, Sol caught/medium | fidelity fail (inferred) | Opus caught/medium, Sol caught/high |

## What this means

On bifrost-dev relation bugs, supplying the complete decisive fact is close to supplying the bug. For 10 of the 17 diffs, no wording within three rounds kept every element of the fact and avoided the Opus/Sol pair flagging the defect. Most were flagged caught or partial in every round they reached the judges. That limits the earlier results:

- Round L's OT lift (+27.5 points) came from briefs that 15 of 17 leak judgments flagged.
- The no-leak arm's ON − AF lift (+37.3 points) passed a weaker leak rule, Opus-only `missed` at any confidence, with edits that often dropped elements of the fact. This arm shows that keeping the fact complete under the stricter Opus/Sol rule makes most briefs leak again.

So neither lift can be credited to a non-leaking decisive fact. Both measure briefs that state, or come close to stating, the defect. Nothing more is claimed here. For the 6 editor-failure diffs the question stays open: their editors never produced a fidelity-passing candidate in the final round, so the leak judges never tested whether their complete facts can be stated neutrally.

## Cost

These are recorded API-equivalent estimates at 10^10 ticks per dollar; all sessions ran on claude7 or codex.

| Stage | Sessions | Estimate |
|---|---:|---:|
| Editors, rounds 1–4 (Opus/high) | 66 | $16.68 |
| Fidelity checks, rounds 1–4 (Opus/high) | 4 | $4.22 |
| Leak judges, rounds 2–4 (Opus + Sol) | 6 | $2.66 |
| Cause check, round 2 (Opus/high) | 1 | $0.16 |
| **Total** | 77 | **$23.72** |

No reviewer, AF2 or round N sessions ran. codex4 was not used.

## Process notes

- The harness is `brief_oracle.py --strict`: editor instructions in `brief/STRICT.md`, fidelity in `brief/FIDELITY.md`, the cause question in `brief/CAUSE.md`. It also uses `verify_driver --judges` for split judging. The fidelity checker, cause checker and every judge side are checked against their recorded producer (profile and model).
- One verify_driver bug delayed the round 3 leak launch: lock-file handles shadowed the driver's thread lock. No session ran on the broken code. One round 1 editor finished but was misread as waiting for input; the driver was fixed and the unit collected.
