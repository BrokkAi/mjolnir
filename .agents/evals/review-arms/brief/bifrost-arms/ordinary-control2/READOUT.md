# Ordinary-control result

The registered Control2 passes. Confirmed distinct groups per reviewer are **0.650 without a brief** (78 group incidences/120 reviewers), versus **0.717 with a blind Sol brief** (86/120): **110.3% retention**, above the 90% bar. The paired 95% bootstrap interval for the per-reviewer difference is −0.050 to +0.175 groups. This supports the registered retention check, not a precise gain estimate.

The single pooled, hidden-arm Sol/high pass covered all 192 findings from 29 diffs and all 240 reviewer sessions, including 105 zero-finding handbacks and 11 diffs with no findings. It produced 69 distinct groups: 54 confirmed, none plausible and 15 rejected. These are source-reading judgments under the registered SPEC, which forbids builds/tests; no second pass was added. A technical audit found no observed violation in270represented requests, but14read/search calls lack complete arguments, so fully clean runtime behavior is not certified.

The original broad matcher selected eight rejected BT findings. The parent read each full finding and its frozen brief: seven build on/restated a brief relation, one shares a generated-file anchor without the claimed relation. The registered ceiling is eight for 40 diffs, so this check also passes. Two duplicate-pair groups contribute separate finding IDs as the method requires. The marks do not prove what the reviewer read or that a brief relation was false. Exact quotations and reasons are in HANDMARKS.md and handmark-evidence.json.

All 15 verifiers are API-confirmed suspended. Estimated API-equivalent verification cost is $3.8380024 with complete usage coverage, not a subscription charge. Ordinary review image changes and unavailable reviewer tool audits/latency remain execution limitations. Unknown-truth controls and zero-finding outputs are not confirmed-clean cases.

The negative relation-stage primary reading remains unchanged. Passing retention and the reliance ceiling does not satisfy the failed primary uplift bar; the registered overall reading remains “Does not generalize.”
