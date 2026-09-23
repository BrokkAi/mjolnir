# Jev turn-evidence comparison, 2026-09-20

Latest-user scope plus no transcript tool calls (`0+1`) was the strongest simple variant in both comparisons. In the strict pilot it classified all four real evaluation points as expected above the unchanged 0.85 threshold. In the subsequently authorized twenty-turn exploratory set it matched all eighteen unambiguous labels, versus sixteen for each other variant. A relevance-selection call did not improve accuracy and added latency. These are a small strict pilot and a separate incomplete-runtime comparison, not a thirty-turn strict benchmark or held-out accuracy result.

## Corpus and provenance

The requested target was thirty real turn cases. Luna nominated sixty turns from historical SQLite data, but the historical API events do not retain Jev's exact queue/background counters or foreground-tool inventory. The user explicitly required complete runtime evidence, so those nominations were excluded from scoring.

A scan of nine worker logs, forty-nine relay journal files, daemon logs and diagnostics found exact Jev evidence in only the bifrost2 worker log. Six requests were present; requests 3 and 6 were retry evaluations of unchanged conversations and were omitted. Requests 1 and 2 concern the same merge/instruction-edit turn, in Running and Replied phases. Requests 4 and 5 concern a later continuation turn at two different stages of progress. All four belong to the development set; there is no held-out set.

The original checkpoint at frontier 116381 preserves the merge conversation. Its final assistant message was marked non-streaming after the first logged request; for that point, the message text is sourced verbatim from the actual captured request, whose entire assistant text exactly matches the checkpoint. Every other included transcript item's last change predates the request. Later points use the existing Rust event projection to replay consecutive journal records from that checkpoint through the captured request timestamp, validating event digests and ordinals. The resulting latest user and assistant messages match the captured request. Runtime evidence is copied from the original requests, not inferred from transcript silence.

Labels were reviewed before requests were sent. The running merge point has a real ambiguity: the requested merge and instruction changes were finished, but broader implementation had resumed and one background command remained. Its expected label is completion of the latest request. The Replied point has zero captured foreground/background/queued work. The other two points explicitly describe continued fixes/investigation and are labeled StillWorking. This ambiguity and the shared-session correlation limit any accuracy conclusion.

Six separately identified synthetic controls cover completion, pending work, a real approval question, a quoted approval question, misleading old tool text, and intermediate progress. Synthetic results are not counted as additional real turns.

## Variants and protocol

Baseline uses the existing shared summary over all available conversation. `0` starts at the latest delivered user message. `0+1` additionally removes all transcript tool entries before budgeting, retaining the original separate live work facts. `0+2` makes one Jev request with a Noul relevance question for each tool candidate, retains scores >=0.5, then makes the ordinary classification request with those calls and the conversation. It does not rewrite calls or pass selection scores to the classifier.

The selector sees only calls present in its bounded rendered input. If candidate questions would exceed the 64 KiB request limit, rendering is reduced and the candidate list recomputed. It evaluated sixteen calls for each merge point and eight for each ongoing-work point. The full turn contained sixteen, sixteen, fifty-six and 313 calls respectively; omitted calls were not scored. This budget effect is part of the measured two-call strategy, not an unconstrained relevance oracle.

The four variants each ran once per case, with shuffled ordering and concurrency two: fifty HTTP requests total, no errors. Every response identified `jev-1.13.0`. Classification questions and runtime fields were held fixed; the threshold and production decision function were unchanged. Raw inputs, responses, token usage, request hashes, selector scores and timing are retained locally.

## Results

Every variant returned the expected category at all four real points. The table gives the API's confidence, not calibrated correctness probability.

| Evaluation point | Expected | Baseline | 0 | 0+1 | 0+2 |
| --- | --- | --- | --- | --- | --- |
| Merge, Running | Finished* | 47% | 93% | 88% | 88% |
| Merge, Replied | Finished | 54% | 97% | 97% | 98% |
| Admission fix, Running | StillWorking | 67% | 82% | 90% | 76% |
| Failing tests/investigation, Running | StillWorking | 65% | 73% | 95% | 88% |

*Requested-action completion with the broader-work ambiguity described above.

| Metric, four real points | Baseline | 0 | 0+1 | 0+2 |
| --- | --- | --- | --- | --- |
| Confidence >=85% | 0/4 | 2/4 | 4/4 | 3/4 |
| Wrong high-confidence categories | 0 | 0 | 0 | 0 |
| Median summed HTTP round-trip time | 0.601 s | 0.609 s | 0.399 s | 1.018 s |
| Median offline end-to-end time | 1.039 s | 1.016 s | 0.794 s | 2.052 s |
| Total request bytes | 212,520 | 169,759 | 24,591 | 204,522 |
| Total input tokens reported | 62,595 | 50,262 | 6,534 | 60,200 |

Offline end-to-end time includes subprocess invocation and parsing snapshots; it is not an estimate of integrated production latency. HTTP times include both selector and classification requests for `0+2`.

The selector retained no calls for either merge point and only the running compaction call for each ongoing-work point (relevance 0.58 and 0.65). Thus it largely converged on removing history but paid for another request. The 97% versus 98% completion difference is a single observation and is not evidence of an improvement.

All six synthetic controls were correctly classified above threshold by all four variants. There were no observed wrong production actions. For Running/Finished and Running/StillWorking, production keeps the current activity regardless of confidence; the important actual action difference here is the post-reply merge point, where baseline abstains and all scoped variants infer idle.

## Additional twenty-turn exploratory comparison

The user subsequently authorized scoring Luna's next-best twenty turns despite their incomplete runtime records. These are twenty distinct turns across five harnesses, outside the strict bifrost2 session. All are recorded Replied-phase points. Their transcripts were reconstructed from timestamp-checked rows through the recorded completed ordinal; every current-turn body matched the previously curated copy, with no later-mutated rows included. All earlier rows in these selected cases were also available at the cutoff.

For every case and every variant, `silent_for_s`, `tools_in_flight`, `background_commands`, and `queued_commands` are explicitly null. An evidence-availability field says these are unknown historical values, not zero or absence of work. This uses the direct experimental endpoint; it is not a proposed production wire contract. These twenty points cannot establish how production would behave with complete runtime inputs.

Final-message labels were reviewed before replay. Two cases were marked ambiguous before requests: a completed milestone under an ongoing burn-down request, and an instruction to start other validation while a base census runs. They remain in the raw results but are excluded from primary accuracy counts. The eighteen scored labels comprise twelve Finished, five StillWorking, and one BackgroundWork. There are no real User cases in this set. All cases are exploratory/development, with no held-out claim or prompt tuning during the run.

Each variant ran once: one hundred additional successful HTTP requests, zero errors. The combined experiment therefore made 150 requests. All replies identified `jev-1.13.0`.

| Metric | Baseline | 0 | 0+1 | 0+2 |
| --- | --- | --- | --- | --- |
| Category matches, 18 unambiguous labels | 16/18 | 16/18 | 18/18 | 16/18 |
| Correct categories at confidence >=85% | 13/18 | 15/18 | 17/18 | 15/18 |
| Below-threshold responses | 5/18 | 3/18 | 1/18 | 3/18 |
| Wrong categories at confidence >=85% | 0 | 0 | 0 | 0 |
| Median summed HTTP time, all 20 | 0.602 s | 0.580 s | 0.453 s | 0.937 s |
| Total input tokens, all 20 | 308,615 | 255,210 | 69,861 | 274,069 |

Two concrete mistakes explain the category differences. Case 15's latest message says it will write a grammar-equivalence differential test; case 16 says it will write new in-crate tests. Baseline, 0, and 0+2 all answer BackgroundWork at low confidence. Removing tool history yields StillWorking at 99% and 100%. The selector retained two calls in case 15 and one in case 16; those retained calls did not resolve the confusion.

There is also a counterexample to treating removal as universally beneficial. Case 5 reports a filter implementation and passing tests, but explicitly says it is not deployed. `0` answers Finished at 89%; `0+1` and `0+2` answer Finished at 58% (the selector retained no calls), and baseline gives 39%. Thus 0 would infer idle while 0+1 would abstain on this case. Higher overall category agreement does not mean every individual decision improves.

The one unambiguous background-work case explicitly says it is waiting for five integration suites; all variants answer BackgroundWork at 100%. The two ambiguous cases received Finished for the milestone and BackgroundWork for parallel validation across all variants; they are reported separately, not retroactively relabeled to improve scores.

## Recommendation and limitations

Prefer `0+1` for a subsequent production change: current-request conversation with transcript tool entries removed, while retaining live work facts. The selector's extra cost is not justified by these observations. After reviewing both comparisons, the user selected 0+1. The implementation now applies it when constructing Jev evidence, using the same shared selection helper as the offline adapter. Separate live work facts and the 0.85 threshold are unchanged. Confirmed steering advances the user boundary in live collection and hot-journal replay; queued/unconfirmed prompts do not. Other mj consumers retain their eight-full-call policy. No proxy deployment or installed-worker replacement was performed.

The strict corpus is too small and correlated to establish general accuracy. The additional twenty turn cases add variety but lack runtime evidence and real user-input cases; the six synthetic controls do not replace that missing coverage. Both comparisons support trying 0+1, while the deployment-status counterexample argues for preserving the conservative threshold and monitoring abstentions.

## Reproduction and implementation

Artifacts are under `/mnt/optane/mj-jev-turn-experiment-20260920`: `manifest.json`, `prepare_strict.py`, `journal-source-hashes.json`, `results/`, `results-report.json`, and `mining/`. The latter contains the read-only SQLite extractor, initial sixty nominations, strict evidence inventory, and next-best-twenty curator/output. The additional comparison is in `next20-manifest.json`, `prepare_next20.py`, `next20-results/`, and `next20-results-report.json`; `next20-run-source.py` preserves the exact replay script before report-only refinements. Raw transcript payloads are not committed.

Build the offline adapter from the repository root with `cargo build -p brokk-mj-transcript --example jev_evidence_probe`. Validate the frozen manifest with `python3 scripts/jev-evidence-experiment.py prepare /mnt/optane/mj-jev-turn-experiment-20260920/manifest.json`. Replay uses `run ... --output <directory>`; the existing results directory resumes only missing jobs with matching hashes. Use a fresh directory for a deliberate new experiment. `report ... --output <directory>` regenerates `<directory-name>-report.json` alongside the result directory without calling TypeSafe. The report excludes premarked ambiguous labels from accuracy and includes phase-specific breakdowns. The script resolves the existing TypeSafe key without printing it.

The adapter reuses the production projection and renderer. `render_with_ids` exposes retained entry identities alongside exactly the existing rendered text, so selector questions cannot refer to history removed by the byte budget. No database migration, new endpoint, wire-schema change or live configuration edit is involved. Jev now uses the selected 0+1 evidence policy.

Validation: all dev-profile workspace tests and all-target clippy passed. Final focused transcript/example tests passed 98 + 2 cases; eight Python behavior tests cover filtering, visible candidate identities, queued boundaries, future-update rejection, unknown facts, probability validation, ambiguity exclusion, and phase/threshold actions. Rustfmt and diff checks passed.

Final adoption check: the shared production selection exactly reproduces all thirty evaluated 0+1 payloads offline. The final rebuilt worker suite passed 531 tests with 9 ignored; other workspace targets passed. The earlier concurrent workspace run used an obsolete test fixture with invalid command IDs; that fixture was corrected and the entire worker target rerun. Final clippy and formatting checks passed.
