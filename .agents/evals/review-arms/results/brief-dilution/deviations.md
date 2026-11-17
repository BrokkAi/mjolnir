# Dilution arm: deviations from brief/PLAN-dilution.md (record for the README)

1. Editor form example: the first generated EDITOR text used a line from terminal-id-reused's
   Bifrost brief as its form example (`hel.hel_acp.drive`, the anchor of that diff's decisive
   item). Replaced with a neutral `crate_name.module.Type.method` example before any editor ran.
2. Truth-check units: lists longer than 175 siblings were split into units of 175 (the largest
   of the other nine lists is 174); symbol-change-keeps-cached-transcript (1,550) became 9 units.
   The 20% false rule applies to the diff as a whole.
3. Truth-check model: 13 of 18 units ran on gemini-4-carbon/high (morannon-podman). The other 5
   (web-feed-self-deadlock, terminal-id-reused, symbol-change--04, --06, --08) stopped when the
   `gemini` profile's weekly quota ran out (sessions answered "Individual quota reached" and ended
   without output); they were rerun on Opus/high (claude4, localhost, blinded checkout
   verify/repo) to keep Carbon quota for round H's judges. Failed session records are in
   truth/sessions/failed-gemini-quota/.
4. symbol-change-keeps-cached-transcript's editor listed 1,550 siblings (others 91-174); kept per
   the plan's "no budget". The readout adds a post-hoc sensitivity line without that diff,
   labelled as post hoc.
5. Truth labels, hand decision: a quoted literal whose only mismatch is collapsed runs of spaces
   counts as true (most verifiers, Carbon and Opus, labelled these true; two marked them false).
   Overridden false -> true: symbol-change-keeps-cached-transcript:930 (--05) and :1107, :1133,
   :1134, :1163, :1164 (--06). Originals kept as truth.jsonl.orig. Falses with any other reason
   (a condition, a scope, a value) stay false (e.g. :596, :700, :953).
6. Duplicate check: split into per-diff units of at most 175 siblings (18 units), and run on
   Opus/high (claude4, localhost) instead of Carbon, because both gemini profiles were out of
   quota (gemini weekly, gemini2's 5-hour window) and Carbon is kept for round H's judges.
7. Contains check: satisfied by construction instead of a judge pass. Arm D's item(s) passed the
   stage A contains rule (`bifrost-plus-check/contains/contains.jsonl`, all yes), and each W brief
   contains them verbatim exactly once (checked by script; build also asserts every line of arm
   D's brief is present). Leak check runs as planned (judge/SPEC.md, two Opus/high judges on
   claude4, because Carbon quota was out).
8. Leak check outcome: the two Opus/high judges did not answer "missed" for every brief (A: 2
   caught, 7 partial, 1 missed; B: 8 partial, 2 missed). Every caught or partial verdict quotes
   arm D's own decisive item (word overlap 0.92-1.00; the best-matching sibling 0.15-0.73), and no
   verdict cites a sibling. Arm D's briefs passed the same check with the ablation's Carbon judges
   (all missed, results/brief-ablation/bifrost-plus-leak.jsonl), so the difference is the judge
   model reading a mechanism fact as partial, not a sibling stating the bug. Under the plan's rule
   ("a leak is traced to its sibling, which is dropped") no sibling is dropped. Judge A stopped on
   a permission prompt after writing its verdicts; they were fetched by hand
   (leak/out/verdicts-0-A.json notes this).
9. Codex credit gate: run 1 (all 20 jobs) was refused at `mj review start` because the private
   instance's `codex` profile lacked `load_balancing_may_consume_credits` (a gate the daemon
   replaced on 2026-11-16 now enforces; the ablation ran before it). Enabled on [profiles.codex]
   only (backup config.toml.before-credits-20261116), per Jonathan's authorization to spend paid
   Codex credit on these runs. Run-1 carriers destroyed; dirs in failed-attempts/dilution-run1/.
10. symbol-change-keeps-cached-transcript is excluded from arm W: its 300,915-byte brief made the
   carrier's historical context 302 KB, which the mj API refuses (413 Payload Too Large), so it
   cannot reach a reviewer in this harness. W runs on the other 9 diffs (dilution-jobs-run2.json),
   and the readout compares W, D and F on those 9 (a paired comparison on the same diffs). The
   undeliverable brief is itself a result: an unbudgeted complete listing does not fit.
11. Reviewer capture: mj 8fe3c40bd (in the daemon restarted 2026-11-16 14:50Z) destroys each
   reviewer session when its role ends, so driver.py's collect() could no longer read reviewer
   sessions or transcripts; run 2 (18 jobs, reviews completed but uncaptured) and canary 1 failed
   in collect. driver.py now reads each reviewer's handback from the store's subagent_handbacks
   (read-only SQLite), usage from `mj usage`, model from session_turn_selections and effort from
   the review status line (diagnosis and patch: child e3bcb454). Reviewer transcripts are no longer
   saved for these arms. Run-2 and canary-1 dirs are in failed-attempts/; all jobs rerun.
