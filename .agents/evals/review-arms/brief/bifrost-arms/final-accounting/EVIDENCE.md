# Completed-stage accounting before Control2 verification

654 unique role SIDs: 514 noncarrier (94 Sol/high, 420 Terra/high) and 140 DeepSeek carrier/high. No SID duplication across roles/stages. Sixteen duplicate manifest/copy sets reconcile by SID; 643 aliased rows agree on available root-cost/coverage fields and raw roots. Only one canonical root aggregate per SID is counted; page/turn arrays are not summed.

| Cohort | Role | SIDs | Fully priced sessions | Supported priced-turn subtotal (API-equivalent USD) |
|---|---|---:|---:|---:|
| primary | carrier | 34 | 17 | 1.7958332480 |
| primary | catch-judge | 6 | 6 | 0.6706140000 |
| primary | editor | 4 | 4 | 0.2457864000 |
| primary | handcheck | 1 | 1 | 0.2153700000 |
| primary | leak-round0 | 2 | 2 | 0.3563800000 |
| primary | leak-round1 | 2 | 2 | 0.2037592000 |
| primary | reviewer | 102 | 102 | 4.6051552000 |
| primary | writer | 17 | 17 | 3.5996476000 |
| secondary | carrier | 26 | 17 | 0.7681525280 |
| secondary | catch-judge | 4 | 4 | 0.5597000000 |
| secondary | handcheck | 1 | 1 | 0.1574784000 |
| secondary | leak | 2 | 2 | 0.2585964000 |
| secondary | reviewer | 78 | 78 | 3.2510356000 |
| secondary | writer | 13 | 13 | 3.2375960000 |
| ordinary-writers | writer | 40 | 40 | 7.7348608000 |
| ordinary-reviews | carrier | 80 | 55 | 3.8130715040 |
| ordinary-reviews | reviewer | 240 | 240 | 10.0092940000 |
| auxiliary | H6-applicability | 1 | 1 | 0.0870680000 |
| auxiliary | contains | 1 | 1 | 0.1833440000 |

All five expected cohort subtotals independently match raw root cost ticks: primary11.692545648; secondary8.232558928; ordinary writers7.7348608; ordinary reviews13.822365504; separate auxiliaries0.270412. Supported recorded priced-turn subtotal41.75274288. Complete session subtotal37.403001616 covers603fully priced sessions. The4.349741264 difference is supported priced turns in51incomplete carrier sessions, not an estimate for their missing work.

All three carrier cohort totals and the full campaign total stay null. Aggregate persisted coverage:917recorded turns,893full reports,24missing reports,57unfinished turns. The51incomplete session IDs are in pre-verification.json; these are coverage gaps, not a claim of live carriers. No quota-dollar conversion or subscription-charge assertion.

Carrier/root separation: all140 saved raw carrier roots have their own expected SID, DeepSeek-only by_model/turn_selections, model token totals matching root tokens, and no reviewer-child commands in captured turn pages. Reviewer roots belong to420 distinct Terra SIDs. carrier-root-samples.json retains six bounded full metadata samples, two per review cohort. These artifacts corroborate separate recorded usage. Raw scalar cost has no child-inclusion flag or model-specific dollar allocation, so cost aggregation implementation is not independently proven here; addition of carrier scalar cost assumes it describes the same-SID usage presented by that root. No duplicated reviewer cost evidence found. Missing usage cannot be attributed from artifacts.

Excluded entirely: pending Control2, M1 corpus/census/tracing, parent/helper orchestration. No partial ordinary verification outputs/keys, live CLI, network, service/config/source/test/build or commit actions. Only new accounting artifacts written. Existing frozen files/readings untouched.

Artifacts: pre-verification.json (source manifest hashes, aggregate/role coverage, gaps, alias checks); role-ledger.jsonl (one row/SID with raw usage path/hash, scalar cost, coverage, selection/provenance); carrier-root-samples.json (scope corroboration). No contradictions found.
