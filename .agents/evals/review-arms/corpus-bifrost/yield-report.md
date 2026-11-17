# Frozen corpus yield and qualification reasons

**30 selected from canonical positions 1–127 (23.6% selected yield).** One additional group, **122**, qualified but was excluded for spread; **96** did not qualify or remained unconfirmed. Including the spread exclusion, qualification yield is **31/127 (24.4%)**. The frozen first 48 contains **9 qualified/selected cases**, not the earlier provisional six.

This reports source selection only. It contains no evaluation census outcomes or model results, and changes no frozen selection.

## Final-prefix accounting

| Source prefix | Groups | Selected | Qualified, unselected for spread | Not qualified / unconfirmed |
|---|---:|---:|---:|---:|
| 1–12 | 12 | 4 | 0 | 8 |
| 13–24 | 12 | 2 | 0 | 10 |
| 25–36 | 12 | 2 | 0 | 10 |
| 37–48 | 12 | 1 | 0 | 11 |
| 49–60 | 12 | 4 | 0 | 8 |
| 61–72 | 12 | 4 | 0 | 8 |
| 73–84 | 12 | 3 | 0 | 9 |
| 85–96 | 12 | 1 | 0 | 11 |
| 97–108 | 12 | 2 | 0 | 10 |
| 109–120 | 12 | 4 | 0 | 8 |
| 121–127 | 7 | 3 | 1 | 3 |
| **Total** | **127** | **30** | **1** | **96** |

Final parent decisions override the earlier source proposals at **97** and **105** (explicit lead SPEC 4 checker/validator-only exclusions) and **125** (performance-only cache retention; the current proof/graph remains available). **34** uses accepted intro `69e3afb5d2c4a2799c14e9d5e092b8619c9f8e1f`; its historical `82151e1e5636bfc3215ccce8ed64b079b68f1dc4` attribution remains rejected. These are corrections within their original draw positions, not extra draws.

**122 is not a failed bug record.** It is the third qualified inverse-usage routing case after 77/117 and was excluded under SPEC 8. Position 127 supplies the next selected case.

## Documented failure reasons

Primary counts partition the 96 unqualified groups. Secondary counts are additional, nonexclusive flags from the same recorded reasons; they may concern another examined member or an alternative boundary in that group. No source was retraced and no absent cause was inferred.

| Primary documented obstacle | Primary groups | Additional secondary flags | Any documented flag |
|---|---:|---:|---:|
| Cutoff (SPEC 1) | 11 | 4 | 15 |
| Introduction over 4,000 lines (SPEC 7) | 20 | 10 | 30 |
| Preflagged / declared limitation (SPEC 5; intent at intro) | 10 | 8 | 18 |
| Not established product behavior (SPEC 4) | 20 | 17 | 37 |
| External fact / intro findability (SPEC 3) | 6 | 3 | 9 |
| Unconfirmed attribution / exposing boundary (SPEC 2/3/6) | 29 | 14 | 43 |

Performance-only cases explicitly recorded without product correctness failure: **101, 103, 125**. Six prefix attempts explicitly retain source status `unresolved` (2, 8, 11, 13, 14, 20); additional rejected records also describe bounded attribution uncertainty. There are no explicitly deferred or unattempted positions inside 1–127. A rejected group does **not** mean every marker in it was disproved. Verbatim source reasons, final parent reasons and recorded unexamined-member notes are preserved in the JSON.

## Source correction and preparation overhead

The initial pass considered **22 comment markers** and missed **12,932 canonical attributes**. The corrected source catalog has **4,168 canonical groups**, shuffled with seed **20261118** (order SHA256 `20123d27b1ade9e3a30a9014a7d4242065c29f77a17dbccea775eb202063f7cf`). The fallback **887-issue order was not used in the final sample**. These catalog-construction facts are supplied by the lead; this report does not reopen or recount inventories. It directly counts the source attempt records.

| Separate preparation population | Records / slots | Recorded status counts | In final-prefix yield? |
|---|---:|---|---|
| superseded_issue_fallback | 72 | accepted: 20; unresolved: 6; rejected: 37; confirmed: 7; unattempted: 2 | No |
| initial_comment_only | 22 | rejected: 15; duplicate: 3; accepted: 2; unresolved: 2 | No |
| canonical_groups_beyond_final_prefix | 41 | confirmed: 10; rejected: 18; deferred: 4; accepted: 2; unattempted: 7 | No |

The old issue proposals and comment-only proposals are historical statuses, not audited final-prefix qualification. The 41 extra canonical slots (128–168) contain 12 source proposals marked accepted/confirmed, 18 rejections, four deferred partial traces and seven explicitly unattempted entries. Five of those unattempted entries have preliminary intro SHAs; they still do not represent completed tracing. No claim is made about unrecorded positions 169–4,168. Populations overlap, so their counts are not additive independent bug discoveries.

## Frozen source and discovery provenance

All **30 selected records have `source="hard-won"`**. Their frozen `found_by` strata are:

| Discovery metadata | Count | Meaning |
|---|---:|---|
| Known-empty `[]` | 17 | Known metadata, no recorded found-by label; not proof of human-only discovery. |
| Known nonempty | 1 | `found-by:policy-oracle` (position 62). |
| Unknown `null` | 12 | Unknown; preserved separately from known-empty. |

Original Mjolnir records do not contain these discovery fields, so no missing provenance is imputed.

## Original-record size and bug-kind comparison

Changed-line counts use the existing registered fields; source diffs were not recomputed. Known-bug exclusions use the recorded golden/fixture/lock exclusions.

| Registered population | N | Median changed lines | Range | Over 1,500 |
|---|---:|---:|---:|---:|
| Bifrost frozen known bugs | 30 | 827 | 109–2936 | 8 |
| Bifrost registered ordinary diffs | 40 | 387 | 91–1473 | 0 |
| Original Mjolnir known bugs | 30 | 688.5 | 11–2236 | 5 |
| Original Mjolnir ordinary diffs | 40 | 326 | 90–949 | 0 |

The Bifrost known-bug median is 2.14× its registered ordinary-diff median; the original Mjolnir ratio is 2.11×. These are descriptive size differences, not evidence about reviewer performance or controlled size matching.

Bug-kind labels retain each original corpus taxonomy. Bifrost has `wrong result` (4), `name resolution` (2), `error handling` (2), and 22 singleton categories, including completeness/proof state, inference, control flow, routing and protocol ordering. Mjolnir has `lifecycle` (3), `configuration`, `UI state`, `security`, `error handling` (2 each), and 19 singleton categories including cancellation, staleness, deadlock, persistence and paths. Ordinary records have no bug-kind labels; none were inferred from subjects. Exact distributions and all original size records are in the JSON.

## Per-position disposition index

`Selected` means present in the frozen corpus. `Spread` means qualified but unselected. Other rows name only their primary documented obstacle; secondary flags and full verbatim reasons follow in the appendix and JSON.

| Position | Canonical group | Final disposition / primary obstacle | Secondary documented flags |
|---:|---|---|---|
| 1 | `issue:432` | Selected | — |
| 2 | `issue:3074` | unconfirmed boundary | external / findability |
| 3 | `issue:1833` | Selected | — |
| 4 | `issue:12128` | not product behavior | cutoff, >4,000 lines, preflagged / declared |
| 5 | `issue:3364` | Selected | — |
| 6 | `commit:445e5b1801d5b479aa28a53291a963a1de444ba9` | not product behavior | — |
| 7 | `issue:1899` | preflagged / declared | — |
| 8 | `issue:1057` | unconfirmed boundary | preflagged / declared, not product behavior |
| 9 | `issue:8618` | Selected | — |
| 10 | `issue:2544` | >4,000 lines | — |
| 11 | `commit:1b531c29a6293421faaf1e154764c736a796a6c2` | unconfirmed boundary | preflagged / declared, not product behavior |
| 12 | `issue:8259` | preflagged / declared | — |
| 13 | `issue:2579` | unconfirmed boundary | >4,000 lines, preflagged / declared |
| 14 | `issue:5868` | unconfirmed boundary | not product behavior |
| 15 | `commit:21df7c3ef361a708bf65c7205de94be9ea40e040` | not product behavior | — |
| 16 | `commit:1daf8988132d445b77ede6f4bd4e47f3408dd38a` | not product behavior | — |
| 17 | `issue:3357` | Selected | — |
| 18 | `commit:a72a3892732ca2b9b932d8a023990519a2dde845` | >4,000 lines | — |
| 19 | `commit:c9b47cfdb9ae28ce870e3d868e0dbaf439e30be7` | >4,000 lines | — |
| 20 | `issue:3041` | unconfirmed boundary | >4,000 lines, not product behavior |
| 21 | `issue:7007` | >4,000 lines | preflagged / declared, not product behavior |
| 22 | `issue:3811` | Selected | — |
| 23 | `issue:7508` | >4,000 lines | — |
| 24 | `issue:4796` | not product behavior | — |
| 25 | `issue:13313` | cutoff | not product behavior |
| 26 | `issue:13215` | Selected | — |
| 27 | `commit:c4eaa33055dc986e5530927dafdd33fbd5128c9f` | not product behavior | — |
| 28 | `issue:2193` | unconfirmed boundary | — |
| 29 | `issue:12936` | cutoff | >4,000 lines |
| 30 | `issue:3503` | external / findability | >4,000 lines, unconfirmed boundary |
| 31 | `commit:bce0502f58e19d49fed2aa4c2f955f817e72887f` | not product behavior | — |
| 32 | `issue:749` | unconfirmed boundary | >4,000 lines |
| 33 | `commit:7a00192bb238375482f80ed202cac348d7360184` | unconfirmed boundary | >4,000 lines, not product behavior |
| 34 | `commit:aa501216592e29e7f6c3ad62c0a61dac82dac368` | Selected | — |
| 35 | `issue:2408` | unconfirmed boundary | — |
| 36 | `commit:5f687c6c4a7660809ff2d9cf723436db291ff863` | preflagged / declared | not product behavior |
| 37 | `issue:2239` | unconfirmed boundary | — |
| 38 | `issue:3490` | external / findability | preflagged / declared, unconfirmed boundary |
| 39 | `issue:13041` | cutoff | — |
| 40 | `issue:1161` | not product behavior | — |
| 41 | `issue:11023` | cutoff | — |
| 42 | `commit:56124738631f7fb72f1c2160fe5a5bdbe498391b` | unconfirmed boundary | — |
| 43 | `issue:11573` | unconfirmed boundary | — |
| 44 | `issue:2157` | Selected | — |
| 45 | `issue:2833` | preflagged / declared | not product behavior, unconfirmed boundary |
| 46 | `issue:10698` | unconfirmed boundary | cutoff |
| 47 | `issue:3777` | >4,000 lines | — |
| 48 | `issue:8799` | cutoff | — |
| 49 | `commit:eea67f99599410a874c7c37dea4fb40b0f9c187c` | Selected | — |
| 50 | `issue:8751` | cutoff | — |
| 51 | `issue:7620` | Selected | — |
| 52 | `commit:ec9ed17871426aa7827936269de4ca3c801d4433` | Selected | — |
| 53 | `commit:83dde99bb1075b264a49020bb823784173b02a83` | unconfirmed boundary | — |
| 54 | `issue:9737` | cutoff | unconfirmed boundary |
| 55 | `commit:8c91d98583fb55ec581933aed249431c80f77423` | Selected | — |
| 56 | `issue:11855` | not product behavior | cutoff |
| 57 | `issue:7627` | >4,000 lines | unconfirmed boundary |
| 58 | `commit:e626e7259df4c177850c0065f0f2c0e07a198852` | not product behavior | — |
| 59 | `commit:77c8926b39c073494d59f2e0cf67bd6512ba03a5` | >4,000 lines | — |
| 60 | `issue:2568` | unconfirmed boundary | — |
| 61 | `issue:10229` | cutoff | — |
| 62 | `issue:5745` | Selected | — |
| 63 | `commit:685c099dc03e0a7bb223f735034b94458a1b408c` | Selected | — |
| 64 | `commit:f5ff17edf1709fcde9f753c2cf92b143cbf5d448` | Selected | — |
| 65 | `issue:1613` | >4,000 lines | — |
| 66 | `issue:1837` | unconfirmed boundary | — |
| 67 | `commit:af5d1f1655b2c390efc14b4b3bc63372712f3ded` | unconfirmed boundary | not product behavior |
| 68 | `issue:2033` | Selected | — |
| 69 | `commit:87975ac9e3b6fddeb5307472d4bcd3b544812f77` | not product behavior | >4,000 lines, unconfirmed boundary |
| 70 | `issue:6556` | unconfirmed boundary | >4,000 lines |
| 71 | `issue:4453` | preflagged / declared | unconfirmed boundary |
| 72 | `commit:75135e29d1c050a2a9b8211d61ffab934a76c184` | cutoff | — |
| 73 | `issue:2071` | unconfirmed boundary | >4,000 lines |
| 74 | `issue:3070` | Selected | — |
| 75 | `issue:415` | >4,000 lines | — |
| 76 | `commit:5a61f8026268fd3e852cbad36039e5b8c0310ddd` | preflagged / declared | unconfirmed boundary |
| 77 | `commit:6074c794e6bd5c4eae8f160e4c938c42f645963e` | Selected | — |
| 78 | `issue:1956` | >4,000 lines | — |
| 79 | `commit:86e7440a87d35df723597ead9de254089156cb46` | not product behavior | — |
| 80 | `commit:28f0b8ec05a48f7589863afa8cb8c2ab24a4b464` | preflagged / declared | not product behavior, unconfirmed boundary |
| 81 | `commit:5f5b353ae5244a484528396232c8bb085d473213` | unconfirmed boundary | external / findability |
| 82 | `issue:12922` | not product behavior | cutoff |
| 83 | `issue:10863` | Selected | — |
| 84 | `issue:11358` | not product behavior | external / findability |
| 85 | `issue:953` | >4,000 lines | — |
| 86 | `issue:3134` | unconfirmed boundary | not product behavior |
| 87 | `commit:bb45edd4de962f4fc229aa15caae3941edd9e250` | >4,000 lines | — |
| 88 | `issue:2601` | unconfirmed boundary | not product behavior |
| 89 | `commit:874e8edbc950bd6ac4bc93913a632ce9697c7c5a` | Selected | — |
| 90 | `issue:1893` | >4,000 lines | — |
| 91 | `issue:12792` | unconfirmed boundary | not product behavior |
| 92 | `issue:12249` | preflagged / declared | — |
| 93 | `commit:ed722fa56803e3b783889067c3407372f4e401fc` | preflagged / declared | not product behavior, unconfirmed boundary |
| 94 | `commit:db4a2c332dd24439eb197d33d717dbdc1df9bc99` | not product behavior | unconfirmed boundary |
| 95 | `issue:8304` | >4,000 lines | — |
| 96 | `issue:8310` | external / findability | preflagged / declared, unconfirmed boundary |
| 97 | `issue:1867` | not product behavior | — |
| 98 | `commit:18a38bca1916c70744007806da28b3e3777c64f9` | external / findability | — |
| 99 | `commit:1c12dfc6a0341ab4911e08d97abde920bac48bed` | Selected | — |
| 100 | `issue:4408` | external / findability | unconfirmed boundary |
| 101 | `commit:d0ce89824e2792b3faa4967315ad2e0252fd4312` | not product behavior | — |
| 102 | `issue:11370` | unconfirmed boundary | — |
| 103 | `issue:7573` | not product behavior | — |
| 104 | `issue:1762` | unconfirmed boundary | — |
| 105 | `commit:c5663ff4a73360ddfc182f34ebe05e283150573c` | not product behavior | — |
| 106 | `issue:5331` | Selected | — |
| 107 | `issue:13312` | cutoff | — |
| 108 | `issue:8264` | >4,000 lines | — |
| 109 | `issue:188` | >4,000 lines | — |
| 110 | `issue:1828` | unconfirmed boundary | — |
| 111 | `issue:12761` | Selected | — |
| 112 | `commit:31913fee42a05ec61aadd1f3ca0a76ba1fed51f8` | unconfirmed boundary | not product behavior |
| 113 | `issue:8599` | >4,000 lines | — |
| 114 | `issue:2854` | >4,000 lines | — |
| 115 | `commit:c854959b282948f64831b685e350126573cdf496` | >4,000 lines | — |
| 116 | `issue:2213` | unconfirmed boundary | — |
| 117 | `commit:093b17cd76edaf2c67b3189fd0045262d0b4608b` | Selected | — |
| 118 | `issue:1518` | external / findability | preflagged / declared, not product behavior, unconfirmed boundary |
| 119 | `commit:12f759b2964cdae96832e59b7fe5dafc87a92832` | Selected | — |
| 120 | `commit:e83f35411767aaffedbf1da5092d49e2d4a64b1d` | Selected | — |
| 121 | `commit:69e0507ebf4be4487d9b93020101e470a1b01541` | cutoff | — |
| 122 | `commit:23f2f89b2092448d5d0ae742a5603ed7da352cd8` | Spread only — qualified | — |
| 123 | `issue:7183` | Selected | — |
| 124 | `commit:e3a7cdf04a43b8401f419a39ad6fab4a3743e868` | Selected | — |
| 125 | `commit:e91fd3a11f33b67f6d4bd7fd23976c6766b2e0e8` | not product behavior | — |
| 126 | `commit:654654dd4755dcc5cba9317a045b9d17551942a3` | preflagged / declared | — |
| 127 | `commit:36fa96bd75ca0de467734e93e780bcc177f2a461` | Selected | — |

## Verbatim source and final decision reasons

Each block preserves the exact recorded reason. A final parent override takes precedence over the historical source reason. Position 34’s earlier source reason is retained only as superseded provenance. No linked evidence, audit, issue, source or evaluation files were opened for this report.

<details><summary>1: issue:432 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:1`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
Intro adds whole-file synthetic owners to span-only enclosing lookup; exact same-extent Python module tie changes runtime lookup results.
```

</details>

<details><summary>2: issue:3074 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:2`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
The marker combines old grammar parse failures with a later adapter omission. The Sep10 grammar integration adds the pinned productions but omits adapter support; the old parser did not expose those shapes. A first defective runtime adapter change, distinct from grammar capability newly becoming available, was not attributed confidently from repository-only evidence. Do not force a known later grammar/interface behavior onto an older intro.
```

</details>

<details><summary>3: issue:1833 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:3`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
First marker is a passing non-template control, as original fix explicitly states. Second marker pins the template owner inherited-member failure newly exposed by June19 inherited walk.
```

</details>

<details><summary>4: issue:12128 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:4`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The first marker tests preservation when a new Nov12 async execution-kind vocabulary/migration is added; it is assurance for new capability, not a demonstrated earlier destructive migration. Await interference and must-return markers belong to that new async admission (must-return boundary is post-cutoff). The older async body deferral is explicitly documented as unsupported deferred activation, not an unflagged wrong complete answer. Earliest traced Deferred producer origin is a huge July20 change (79,162 nonfixture changed lines), also outside size if used as intro.
```

</details>

<details><summary>5: issue:3364 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:5`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
New generated compatibility checks are placed after full asset verification on synchronous acquisition; measured minute-scale first-request blocking on already incompatible indexed input.
```

</details>

<details><summary>6: commit:445e5b1801d5b479aa28a53291a963a1de444ba9 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:6`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Named445e5b1801d5 is a tests/changelog assurance checkpoint whose commit and plan explicitly state new tests confirmed the implementation without any production correction. Both attributes were retrofitted later; no actual runtime defect or fix established for either marker.
```

</details>

<details><summary>7: issue:1899 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:7`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Both producer and activation tests address conditions deliberately discarded by the Aug2 API collector: the introducing diff says the pack remains a static surface and never evaluates a guard directly beside unconditional descent through if/try/else. The conditional-surface limitation is already named in the defective diff; adding recorded declaration guards/activation filtering is the later capability expansion.
```

</details>

<details><summary>8: issue:1057 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:8`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
This broad42-marker issue mixes fuzzer grading defects, passing controls and several runtime selector changes. Stable scan: first11 are fuzzer-only; cfg-pair test is a control written with macro item indexing; canonical definition identity belongs to new1136 behavior; generic probe is fuzzer-only; next3 tests passed before their annotated4b85 fix; JS exact-anchor test likewise preserves an existing output. First possible Go anchored ambiguity traces to69ac but its message/comment explicitly preserve exact/top-level priority. Whether that is a pre-existing defect versus a later selector priority contract could not be established in the bounded pass. No later marker was selected past this ambiguity.
```

</details>

<details><summary>9: issue:8618 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:9`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
Actual exposing change is Oct30 retained-domain sharing, not the Oct31 inheritance change. It lets the second fill reuse the existing Java class-index memo without its failed-build gap.
```

</details>

<details><summary>10: issue:2544 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:10`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The multi-sink runtime collision becomes reachable through the production taint adapter9ce1a3aecb5. Exact anchor construction lines are blamed there. Its unmodified parent diff is+4541/-174 =4715 lines after allowed exclusions, exceeding4000. Cannot carve only the taint submodule out of the introducing commit. All6 markers describe the same source-to-sink identity defect/controls.
```

</details>

<details><summary>11: commit:1b531c29a6293421faaf1e154764c736a796a6c2 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:11`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
The destructive result-carrier kill is added in8fde3ccea93, but that commit explicitly states nothing calls the fixpoint yet. Parse-time activation is introduced by mergead12d1bf9a26 (ordinary intro required); later provider/storage integration is staged across more commits. The b26 regression is deliberately ignored and already names the exact defect. A first ordinary runtime-activating change with sufficient existing prerequisites, not preflagged and not a merge, was not established confidently.
```

</details>

<details><summary>12: issue:8259 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-001-012/attempts.jsonl:12`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The Aug26 unknown-conversion location branch explicitly records Value(result)/Values/Unknown with the unavailable-until-conversion-resolution message; adjacent comments already describe how an unread assignment-result gap opens the whole procedure. Issue8259 itself cites that pre-existing warning and says only the identity-known subcase was fixed then. The new conversion-call scaffold repairs a recognized limitation; the marked broad-gap defect is preflagged in its introducing diff.
```

</details>

<details><summary>13: issue:2579 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:1`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
Primary unresolved-target marker traces a targetless arm to an old generic dispatch helper before the modern modelability prerequisites, with no confirmed short first-defective runtime boundary. Later external-effect markers trace to the explicitly limited original effects feature dabd52dc3b5 (4503 lines, above max) whose code says external artifacts remain unmodeled. No eligible alternative confirmed.
```

</details>

<details><summary>14: issue:5868 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:2`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
The first marked test was explicitly rescoped as a direct-helper contract that does not reproduce a runtime recovery region. Its helper did not previously recognize damaged function-shaped namespace heads, but c820d8c6a8 introduced a repair for already broken extraction, not proven onset of the pugixml defect. The public golden proves a later correction; no confident first-defective eligible runtime diff established.
```

</details>

<details><summary>15: commit:21df7c3ef361a708bf65c7205de94be9ea40e040 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:3`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The changed cancellation path and ExperimentalFactEvaluationTask are #[cfg(test)] at the actual named fix; production run is explicitly kept independent as the eager oracle. No user/client runtime defect at this boundary.
```

</details>

<details><summary>16: commit:1daf8988132d445b77ede6f4bd4e47f3408dd38a — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:4`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Named commit adds only a test and explicitly states this already held without a code change. The marker is a control for a proposed module-inventory rule, not evidence of a prior runtime defect. No introducing defective commit is established.
```

</details>

<details><summary>17: issue:3357 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:5`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
Confirmed first qualifying member in stable source order; parent owns final selection.
```

</details>

<details><summary>18: commit:a72a3892732ca2b9b932d8a023990519a2dde845 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:6`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Primary malformed-JavaScript lookup regression reaches the original get_definition fallback through a2497bfc89 (which only changes const to mutable + namespace filtering), e14 module movement and original 1837808855. The original user-facing addition is 9337 nonfixture lines, above the hard maximum. Remaining inverse lookup-only property markers do not establish a distinct eligible first-defective runtime boundary rather than an added capability.
```

</details>

<details><summary>19: commit:c9b47cfdb9ae28ce870e3d868e0dbaf439e30be7 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:7`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The real source-event ordinal collision traces to source_scenario added by the production taint adapter 9ce1a3aecb5, 4715 changed nonfixture lines (>4000). The intervening semantic_site_key refactor 4d548f6fc36 preserves the site-only key and is not onset. Remaining markers are conformance/control tests added by the same fix, whose only production change is source-event ordinal identity.
```

</details>

<details><summary>20: issue:3041 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:8`; original status `unresolved`, disposition `unresolved`.

Source attempt reason (verbatim):

```text
First marker is a budget control written with newly added C label support, not a reproduced preexisting budget defect. The second pins the actual previously unsupported C goto/label round trip. The fix adds query-local label declarations and a new inverse path; no confident eligible first-defective introduction was established. Original generic get_definition introduction is 9337 lines, but do not silently treat every missing C label feature as introduced by that tool commit.
```

</details>

<details><summary>21: issue:7007 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:9`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Kotlin arithmetic was explicitly limited by syntactic scalar guards. The separate inferred-local self-cycle traces to shared local writes c51cdc3a53 (3336 lines), but native Kotlin point consumers then existed only under #[cfg(any(test, feature="test-support"))]. The demonstrated production regression appears at native-default switch 7fa41c7a80, 13337 changed nonfixture lines, with merge8671edb20339 inadmissible. Do not label the older helper as confirmed production onset; no smaller confirmed eligible exposing commit is available.
```

</details>

<details><summary>22: issue:3811 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:10`; original status `accepted`, disposition `accepted`.

Source attempt reason (verbatim):

```text
Confirmed first qualifying member in stable source order; parent owns final selection.
```

</details>

<details><summary>23: issue:7508 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:11`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
The asymmetric branch-terminal conversion code is added in e102717c907, a 52850-line receiver/semantic change, above the hard maximum. Earlier conditional lowering simply sends each operand to the continuation; it contains no branch result transfer or typed conversion terminal. The later 08e009d71c9 substitution of add_gap by session.record_gap is not onset.
```

</details>

<details><summary>24: issue:4796 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-013-024/attempts.jsonl:12`; original status `rejected`, disposition `rejected`.

Source attempt reason (verbatim):

```text
Actual fix f39b067f8b only updates a golden expectation to match the pointer depth intentionally reported by d0f2fcb21355. The marker/fix message explicitly calls the correction test-only; no wrong product behavior qualifies.
```

</details>

<details><summary>25: issue:13313 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:1`; original status `rejected`, disposition `cutoff-and-source-boundary`.

Source attempt reason (verbatim):

```text
All six marker members examined. The first test accompanies the Nov 15 construction-matcher reuse optimization and receipt validation; its parent uncached walk actually recompiles/replaces the matcher, so that parent is not proof of the claimed stale reused matcher after an unreported edit. Rule-directory, transient-failure and publication-race members pin that new optimization’s follow-up races; seed symlink receipt also depends on November receipt changes. Actual relevant code boundaries are after the cutoff. The issue itself is primarily duplicate rule-read performance, not a reason to admit a different older defect.
```

</details>

<details><summary>26: issue:13215 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:2`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Only the first marker’s inconclusive/clean-label half is selected; the later empty-role hard-failure behavior is excluded. The original numeric projection comes from oversized 1e0ec9cf75b, but before this introduction ordinary default inconclusive runs exited unreliable; a830471d19 newly exposes the false label by default. Explicit policy unknown-verdict overrides already allowed advisory exit-zero paths, so this is not claimed as the first possible false-clean label for every configuration. The intro explicitly requests advisory exit zero and uses the symbolic clean exit constant, but does not flag the MCP string-conflation defect. Fix has a three-request real MCP regression with clean/inconclusive/unreliable responses. Current marker coordinates are pinned HEAD coordinates; actual test was added in the Nov 15 fix, which is allowed.
```

</details>

<details><summary>27: commit:c4eaa33055dc986e5530927dafdd33fbd5128c9f — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:3`; original status `rejected`, disposition `test-fuzzer-only`.

Source attempt reason (verbatim):

```text
The marker test and actual fix concern I2 property-fuzzer grouping two independent same-FQ symbols into a false drift violation. Blame reaches 53f8e5a76b0 for the two-part group key. This changes the test checker, not runtime analyzer/navigation behavior; SPEC real-behavior rule excludes test-only defects.
```

</details>

<details><summary>28: issue:2193 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:4`; original status `rejected`, disposition `unresolved-original-introduction`.

Source attempt reason (verbatim):

```text
Actual fix/test b669f4bad5 confirmed; marker itself was retrospectively added by f3f5c9e38df. The enclosing-AST-only template predicate and several alias fallbacks trace to 51e1cab1cbd, but that change also repairs already-existing reference scope recovery. Earlier workspace lookup could already select a same-named alias when a fragmented signature lost ancestry. No evidence establishes which new line first creates the exact wrong-target runtime behavior rather than merely retaining it. Reject uncertainty rather than present the nearest 3244-line recovery commit as confirmed.
```

</details>

<details><summary>29: issue:12936 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:5`; original status `rejected`, disposition `cutoff-or-oversized-original`.

Source attempt reason (verbatim):

```text
Stable member order examined. Scope-bounding, persisted refusal deduplication and bounded-publication members require the new November refusal-subject transport and are post-cutoff. The fourth member also names older b1b77f63a031: incomplete-selector results discarded as a whole. Blame crosses the behavior-preserving 4d548f6fc36 refactor to original production taint adapter 9ce1a3aecb; full diff is 4715 lines after allowed exclusions, over the 4000 maximum. The remaining producer/grouping/quadratic-retention members concern November refusal-subject accumulation, not an earlier qualifying introduction.
```

</details>

<details><summary>30: issue:3503 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:6`; original status `rejected`, disposition `later-grammar-prerequisite-and-size`.

Source attempt reason (verbatim):

```text
Marker attributed to actual namespace-relative AST/path fix in 093013efa3, not retrospective tag 5f283ece90d. That fix simultaneously upgrades PHP grammar and introduces relative_name handling. Parent path parser inferred namespace relativity from a first textual segment and sliced it; blame traces to e102717c907 (52850 changed lines). The new relative_name shape is not available under that original grammar. Neither the enormous original rewrite nor the simultaneous new-grammar fix is a confirmed eligible introduction of this marker’s exact defect.
```

</details>

<details><summary>31: commit:bce0502f58e19d49fed2aa4c2f955f817e72887f — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:7`; original status `rejected`, disposition `test-fuzzer-only`.

Source attempt reason (verbatim):

```text
Both members pin property-fuzzer I2 false positives/controls for weaker-vs-qualified selectors. Actual bce0502f58e1 changes directional comparison only, not product resolution. Its parent status-drift checker is the defective test logic. Reject under SPEC real runtime behavior rule.
```

</details>

<details><summary>32: issue:749 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:8`; original status `rejected`, disposition `unresolved-original-introduction`.

Source attempt reason (verbatim):

```text
Actual fix 54bfeaeb55 and persisted partial-class constructor regression verified; #749 is the fix PR. Constructor branch 0ba5a94f82c falls back to the same csharp_type_outcome as its parent, so it does not establish first loss of sibling-partial lexical lookup. The later persisted definition-provider rewrite a1dd06847b9 and current visibility wrappers were inspected; no removal of a previously working nested lookup was established. The initial get_definition feature 18378088552 is 9337 lines. Do not force either small refactor as the introducing commit.
```

</details>

<details><summary>33: commit:7a00192bb238375482f80ed202cac348d7360184 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:9`; original status `rejected`, disposition `oversized-original-and-old-helper`.

Source attempt reason (verbatim):

```text
Named SHA 7a00192bb238 is discovery plus an ignored failing test, not the fix: it explicitly says generic() was already affected. Actual fix bff239a860 always publishes a type-parameter Identity slot and unignores the test. Missing slot falls through the engine’s nominal fallback: 45ebf3665cf changes alias handling around an already-existing nominal fallback; 4be28bcdec1 adds bound slots but leaves existing unbounded parameters unchanged. The initial generic-binder producer 61947ed027f is 8700 lines after exclusions and was a pre-cutover checkpoint. No smaller confirmed newly defective runtime boundary established.
```

</details>

<details><summary>34: commit:aa501216592e29e7f6c3ad62c0a61dac82dac368 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:10`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Member order respected: the first grouped-type test was added with the fix and its earlier-RHS assertion is a negative control; the later-local-type omission and second generic-receiver-shadow marker trace to older graph shadowing code, but their true first runtime introductions were not confidently established (the old public path was already described as ignoring shadows). The third member is a helper-contract/control test: its original aa501216592e body requires no target and newly records bound-receiver application, but does not itself pin the false query completion. The fourth member is the first fully qualified runtime regression: the real RQL external_typed_method_does_not_invalidate_package_function_contract_discovery query must remain complete and retain only os.Open when embedded.Open is also present. Restrict matching to false result-contract discovery incompleteness, not hypothetical wrong target minting. All needed model and receiver-query prerequisites exist in 82151e1e563. Remaining members are unselected after this first qualification.
```

Final parent-acceptance.json reason (verbatim):

```text
Independent focused audit accepts actual exposing boundary69e3afb; historical82151 record remains rejected.
```

Accepted corrected intro: `69e3afb5d2c4a2799c14e9d5e092b8619c9f8e1f`. Rejected historical intro: `82151e1e5636bfc3215ccce8ed64b079b68f1dc4`.

</details>

<details><summary>35: issue:2408 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:11`; original status `rejected`, disposition `unresolved-original-introduction`.

Source attempt reason (verbatim):

```text
All four markers traced to tests added by 60560d6433 and implementation in 31913fee42. The first is the actual g_autoptr receiver gap; the other three are repair controls. Earlier 7f338e3b9c adds unique _T fallback only after a receiver type is known; lack of g_autoptr parsing already predates it, so it is not the introduction. C header/semantics rollout b203781da5 and the original definition feature were considered, but no exact new unsupported g_autoptr path versus retained older limitation was established. Reject uncertain attribution.
```

</details>

<details><summary>36: commit:5f687c6c4a7660809ff2d9cf723436db291ff863 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-025-036/attempts.jsonl:12`; original status `rejected`, disposition `test-contract-and-preflagging`.

Source attempt reason (verbatim):

```text
Named commit changes this regression’s expected result from no_definition to resolved while production edits concern receiver-depth accounting and ambiguity collapse. The prior test/commit 0749516696 explicitly says const before declaration must keep returning no_definition as a TDZ control. No newly introduced hidden product bug boundary was established for this marker; the old behavior was expressly documented/tested, violating the unflagged-defect requirement if treated as the target. Do not substitute the unrelated receiver-depth fix for a lexical-binder introduction.
```

</details>

<details><summary>37: issue:2239 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:1`; original status `rejected`, disposition `attribution-uncertain`.

Source attempt reason (verbatim):

```text
The regression fixes bare template_type constructor recognition and nearest lexical owner selection. The shape guard predates the member tier (d9dfbb4); constructor handling0ba and structured template resolution4c36 already perform scope-blind selection, while51e/0e6 add later lexical tiers. These are pre-existing defects/support expansions at multiple boundaries, not a confidently isolated introducing change. Rejected rather than blaming the old unqualified-name helper or the later member-tier addition.
```

</details>

<details><summary>38: issue:3490 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:2`; original status `rejected`, disposition `contract-at-intro-uncertain`.

Source attempt reason (verbatim):

```text
The first marker fixes semantic Rust impl-owner containment. The lexical ancestor matcher originates in88c5f86, whose schema/docs explicitly define inside-decl as containment without crossing a callable; the fix adds a distinct semantic owner oracle. Cannot establish that the later expected Rust owner join was the contract at introduction, or isolate a later exposing change. Later members test the same new ownership relation and its edge cases; no qualifying introduction established.
```

</details>

<details><summary>39: issue:13041 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:3`; original status `rejected`, disposition `post-cutoff`.

Source attempt reason (verbatim):

```text
Exact marker defect is the guard-before-glob regression, not the older inert-cleanup limitation.074aecd (2026-11-14local) replaced expression_ownership’s existing Declared classification with the guard-aware bounded classifier, losing the ten findings.0925dd (2026-11-15) restores that branch and retags/extends the golden. Actual introducing change is after cutoff.
```

</details>

<details><summary>40: issue:1161 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:4`; original status `rejected`, disposition `not-product-observable`.

Source attempt reason (verbatim):

```text
The defective Owner(_) return traces to24968ce. Fix6b8ad explicitly documents that the regression is invisible on product surfaces: real methods are routed away from the bulk inverted graph for dead-code and usage_graph does not render unproven_inbound. Its discriminating regression exercises an internal edge counter. No user/client behavior at intro established; SPEC real-behavior rule fails.
```

</details>

<details><summary>41: issue:11023 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:5`; original status `rejected`, disposition `post-cutoff-surface`.

Source attempt reason (verbatim):

```text
Both markers pin the Rust qualifying-type query/policy constraint and alias expansion. The public qualifying-type vocabulary/execution is introduced by357a9f (2026-11-09), with external modeled alias policy coverage in ad7a2d that same day. This exact user-visible constraint did not exist before cutoff; no older equivalent defect was confidently attributed. Do not treat the new November type-query surface as an old alias helper defect.
```

</details>

<details><summary>42: commit:56124738631f7fb72f1c2160fe5a5bdbe498391b — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:6`; original status `rejected`, disposition `attribution-uncertain`.

Source attempt reason (verbatim):

```text
Examined first three stable members: unqualified tuple-pattern inverse hits, shadow/ambiguity control, nested struct initializer field ownership. Marker names561247, which really is a multi-defect fix/regression commit. Missing tuple handling predates aa49f’s tree-sitter rewrite (the replaced regex also recognized scoped uses only); fc2a adds enum-aware associated roles but does not establish introduction of the existing unqualified miss. Nested field test has no clearly matching new defective path isolated from earlier scanner/type-resolution behavior. Rejected after bounded tracing; did not force an old scanner rewrite as introduction. Remaining later members not traced after this group’s uncertainty/budget stop.
```

</details>

<details><summary>43: issue:11573 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:7`; original status `rejected`, disposition `attribution-uncertain`.

Source attempt reason (verbatim):

```text
Marker’s real lifecycle regression is added in3d92e963 (not marker/tag introduction20ecc). Lifecycle plan documents losing overlay publication while carrying preparation evidence and later republishing only proven unchanged inputs.48a5e74 adds November evidence carry; older lifecycle/runtime support is in dc0ba96. Cannot establish when absent publication first became a visible complete-not_found instead of a later async refresh/activation problem. No bounded introducing/exposing series established, so do not blame old runtime default construction.
```

</details>

<details><summary>44: issue:2157 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:8`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
All SPEC conditions statically checked; first member pins the actual defect fixed by e9e10f60.
```

</details>

<details><summary>45: issue:2833 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:9`; original status `rejected`, disposition `preflagged-or-assurance-only`.

Source attempt reason (verbatim):

```text
First marker’s frozen relay test is added by01094c after7ef48c introduced precise copy-return support. Earlier b228cf explicitly says C++ class/template/deduced by-value returns retain conservative gaps; d8763 also documents typed incompleteness outside proved operations. Thus the unknown class-return capability is preflagged at those exposure boundaries. Second marker is an independent-storage assurance test added by01094c without changing production semantics (its external.rs changes are test helpers/cases); no actual aliasing defect/fix introduction was established. Both members excluded.
```

</details>

<details><summary>46: issue:10698 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:10`; original status `rejected`, disposition `exposing-boundary-uncertain`.

Source attempt reason (verbatim):

```text
Fix4b86bfa excludes configured semantic-pack catalog WAL/directory writes from watcher deltas. The visible pending-warm failure depends on invalidating activation after any such delta;7511100’s scoped stale-ecosystem refresh (Nov5) and c651684’s refresh coalescing (Nov4) are later prerequisites. Older AnalyzerOwnedPaths cache-only classification alone does not establish this runtime failure. No pre-cutoff introducing/exposing change proven; reject rather than labeling an old watcher helper the introduction.
```

</details>

<details><summary>47: issue:3777 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:11`; original status `rejected`, disposition `oversized-and-attribution-uncertain`.

Source attempt reason (verbatim):

```text
First marker pins Complete despite parser recovery swallowing an if. Fixa7d9 adds recovered-region facts and propagation; later0375cd is the merged landing. Original structural query/fact collector3ab6ed adds the no-recovery path but is +6778/-2 (no relevant fixture/golden/lock exclusion brings it below4000). Refactors of seed execution are not introductions, and no later smaller exposing change was established. Later markers are controls/index-path variants of the same missing-recovery signal. Excluded conservatively.
```

</details>

<details><summary>48: issue:8799 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-037-048/attempts.jsonl:12`; original status `rejected`, disposition `post-cutoff`.

Source attempt reason (verbatim):

```text
The original replayable memo path d0e847 lands2026-11-01T03:03:26-06:00;88bd1f adds snapshot proof coverage at03:19. The review fix74447f at04:04 adds stable completion gaps to fact-answer digests. This is the actual memo replay exposure, already after the local Nov1 midnight cutoff; older digest helpers alone could not expose this accepted-replay defect.
```

</details>

<details><summary>49: commit:eea67f99599410a874c7c37dea4fb40b0f9c187c — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:1`; original status `confirmed`, disposition `first-qualifying-member`.

Source attempt reason (verbatim):

```text
Meets full SPEC; earlier examined members rejected for reasons recorded in candidate notes.
```

</details>

<details><summary>50: issue:8751 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:2`; original status `rejected`, disposition `cutoff`.

Source attempt reason (verbatim):

```text
The exact defective GO_UNBOUND_ALIAS_INTERFACE_SQL language-wrapper classification is introduced in db9ad26b121 on 2026-11-01 01:46:47 -0500, after cutoff; 32d0a66beb5 fixes it. Do not use older alias helpers.
```

</details>

<details><summary>51: issue:7620 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:3`; original status `confirmed`, disposition `first-qualifying-member`.

Source attempt reason (verbatim):

```text
Meets full SPEC; earlier examined members rejected for reasons recorded in candidate notes.
```

</details>

<details><summary>52: commit:ec9ed17871426aa7827936269de4ca3c801d4433 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:4`; original status `confirmed`, disposition `first-qualifying-member`.

Source attempt reason (verbatim):

```text
Meets full SPEC; earlier examined members rejected for reasons recorded in candidate notes.
```

</details>

<details><summary>53: commit:83dde99bb1075b264a49020bb823784173b02a83 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:5`; original status `rejected`, disposition `uncertain-attribution`.

Source attempt reason (verbatim):

```text
Actual regression/fix is 83dde99bb107. Strict disk decoding predates the apparent blamed read_source helper: e343581adc0 moves an already-strict analyzer read; c04260c11e3 replaces an already-strict file.read_to_string with from_utf8; 4ad9a16ba0d is an analyzer parallelization/refactor boundary; 44d4a263be adds the older generic helper before C++ exists. No confident actual defect/exposing introduction established; do not label a move/helper as introduction.
```

</details>

<details><summary>54: issue:9737 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:6`; original status `rejected`, disposition `cutoff-or-uncertain`.

Source attempt reason (verbatim):

```text
Inspected all 41 marker reasons and test histories in stable order. The retention budget and derivation capture owners are new November 4 code, after cutoff. Most other markers concern later listing certificates, per-unit routes, and request-overlay captures. The old delegate-retirement test was augmented with budget owner assertions only in aaa26ab7145, not its August original. Module-walk split regression explicitly points to post-cutoff 45d2fc4ee6. No eligible pre-cutoff introduction confirmed; remaining older-topology/reporting exposure is uncertain rather than forced.
```

</details>

<details><summary>55: commit:8c91d98583fb55ec581933aed249431c80f77423 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:7`; original status `confirmed`, disposition `first-qualifying-member`.

Source attempt reason (verbatim):

```text
Meets full SPEC; earlier examined members rejected for reasons recorded in candidate notes.
```

</details>

<details><summary>56: issue:11855 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:8`; original status `rejected`, disposition `test-only-and-cutoff`.

Source attempt reason (verbatim):

```text
The authorizer is a test-support derivation fence, not product runtime. Issue and fix identify a fixture-materialization mismatch from 5f52f2db502 (2026-11-10). Excluded for both real-behavior and cutoff.
```

</details>

<details><summary>57: issue:7627 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:9`; original status `rejected`, disposition `size-or-uncertain`.

Source attempt reason (verbatim):

```text
First member pack producer walker is introduced in 1ec35bbcd98 (+7359 -186 excluding fixtures), beyond hard maximum. Actual correcting walk is 43b103b2be and a971a744618 pins producer output/cache rotation. Second marker arity defect spans old count publication 74545a6cb96 and new generic navigation/application code dbfc41af22d; although the latter is a plausible exposing change, the defect-specific runtime boundary is not confirmed, so kept out of candidates.
```

</details>

<details><summary>58: commit:e626e7259df4c177850c0065f0f2c0e07a198852 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:10`; original status `rejected`, disposition `unproven-runtime-defect`.

Source attempt reason (verbatim):

```text
e626e7259df4 adds dense_persisted_membership_and_request_registrations_are_exact alongside a new dense-membership representation/API and query-ownership redesign. No concrete earlier runtime failure pinned by that test was established; preceding 7533bd5d59 already attempted request cleanup. Do not turn the new representation/control test into an invented bug introduction.
```

</details>

<details><summary>59: commit:77c8926b39c073494d59f2e0cf67bd6512ba03a5 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:11`; original status `rejected`, disposition `size-or-uncertain`.

Source attempt reason (verbatim):

```text
77c8926b39c0 genuinely fixes qualified package/object root handling and adds the named regression. Root-sensitive resolver/candidate construction traces to c665f8b52b7 (>4000 changed lines); 7e03ed26e73 adjusts package precedence but the relevant qualified-root omission already predates it. No bounded introducing change confirmed for this composite marker.
```

</details>

<details><summary>60: issue:2568 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-049-060/attempts.jsonl:12`; original status `rejected`, disposition `uncertain-attribution`.

Source attempt reason (verbatim):

```text
Fix 72bf48fe549 and its real public-definition lexical-barrier regression are confirmed. The forward type fallback is older than the current split-file blame e14fdac82d2; c1420d158c5 adds enclosing-scope lookup but preexisting scope-blind fallback could already select unrelated T. No actual introducing/exposing commit confidently isolated; reject rather than attribute to move or older helper.
```

</details>

<details><summary>61: issue:10229 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:1`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected: member-form and quantified Each defects originate November 6 or later. Old named-capture anchor aa7903f predates cutoff but the demonstrated 2,048-binding clone/response amplification depends on later Each multiplicity; no equivalent user-visible pre-cutoff trigger confirmed. Do not backdate to helper. All 18 members screened; remaining identity/capture budget tests concern new Each series.
```

</details>

<details><summary>62: issue:5745 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:2`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Confirmed first marker: repeated full-module mutation inventories newly charged once per base/name exhaust the shared scope budget in a large inert source file, changing simple hierarchy results to incomplete. c173735 extends the public hierarchy regression as well as the selected memo invariant test. Later members not needed for selection.
```

</details>

<details><summary>63: commit:685c099dc03e0a7bb223f735034b94458a1b408c — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:3`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Confirmed public activation API defect, narrowed to caller-supplied independently valid activation evidence being discarded on dependency discovery failure. The marked CLI assertion is later than the API; bfd526 fixes it and pins deterministic missing-go behavior. Named 685c099 instead adjusts exit reliability. 53a242 refactors an already defective early return; original dc0ba96 adds that return.
```

</details>

<details><summary>64: commit:f5ff17edf1709fcde9f753c2cf92b143cbf5d448 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:4`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Confirmed first marker: original public maintainability report silently hits max_files without setting truncated when another distinct file remains. f5ff17 is the original implementation, not the October fix. Declaration-cap member not selected after first qualifying file-cap defect. d76655a2 is the examined extraction/helper consolidation, inheriting the original f5ff17 missing-truncation behavior.
```

</details>

<details><summary>65: issue:1613 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:5`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected: missing field-selector receiver inference is inherited by original whole-workspace graph d0a2ca5. That introduction exceeds 4,000 non-fixture changed lines. 0f05e93 and d21b8c2 add constructor refinements but are not proven introduction of missing field-derived local receiver behavior.
```

</details>

<details><summary>66: issue:1837 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:6`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected attribution uncertainty: include-driven claims are a newly implemented source-language capability; old extension-filter/tree-sitter code was moved/refactored. ca70ab5 and 08a48b5 are examined boundaries, not confirmed first exposed C++ omission. No defensible exact introduction of user-visible include-claimed .inc behavior. Eight members inspected; unreferenced-inc and other-language extension cases are controls.
```

</details>

<details><summary>67: commit:af5d1f1655b2c390efc14b4b3bc63372712f3ded — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:7`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected: earliest selected Scala native-point implementation 367757 is explicitly cfg(test or test-support), and 4ea7c3 rearranges those trial contexts. Did not prove runtime exposure before boundary fix 41d8f93. Named af5d1f is a merge and not the first defective implementation. Both members depend on the native/trial projection.
```

</details>

<details><summary>68: issue:2033 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:8`; original status `accepted`, disposition `confirmed`.

Source attempt reason (verbatim):

```text
Confirmed narrowed indexing defect: initial Rust analyzer records enum_variant as one field but does not visit its field_declaration_list. Named fields under struct variants are missing from public declarations. Later initializer/usage-resolution repairs are outside this candidate. 1f39128 is a source split, not intro.
```

</details>

<details><summary>69: commit:87975ac9e3b6fddeb5307472d4bcd3b544812f77 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:9`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected sole marked member is explicitly a negative control (instance target must retain its receiver gap), not a test pinning the false gap for a static target. Fix has an unmarked positive test, but the canonical source attribute is on the control. Original general implicit-receiver boundary 83af5b also exceeds the size limit and no smaller runtime-exposing intro confirmed.
```

</details>

<details><summary>70: issue:6556 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:10`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected attribution uncertainty: 77506f4 introduces temp-backed reader pool with deliberate single-connection fallback, but old store already used one mutex. Deadlock requires later selected/native operation retaining reader and nested Go package lookup; 5c5d4e introduces the latter but is oversized and exact retained-reader exposure not established. All three pins concern fallback selection, symlink temp roots and template failure; cannot backdate later nested-read hang to old fallback alone.
```

</details>

<details><summary>71: issue:4453 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:11`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected conservatively: original caller fd90c94 worked with old shared semantic coordinate lookup (no authority rows simply allowed). Actual new panic boundary is 2fac53a, whose commit message explicitly discusses RustDeclarationAuthority having no membership writer, shared keys answering nothing, and adds assertion requiring no such shared keys. Potentially violates SPEC not-preflagged rule. Exact boundary now traced; parent may adjudicate this contract-comment/preflag distinction. No confirmed candidate forced.
```

</details>

<details><summary>72: commit:75135e29d1c050a2a9b8211d61ffab934a76c184 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-061-072/attempts.jsonl:12`; original status `rejected`, disposition `excluded`.

Source attempt reason (verbatim):

```text
Rejected cutoff: actual introducing change 75135e2 is November 10, 2026. It globally treats any listing omission as incomplete JVM inventory; fix 93b89b3 distinguishes manifest-relevant omissions. Both markers cover same late defect.
```

</details>

<details><summary>73: issue:2071 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:1`; original status `rejected`, disposition `unconfirmed-first-runtime-introduction`.

Source attempt reason (verbatim):

```text
The exact fix and map-key owner omission are real, but no first runtime introduction was confirmed. e36282ee691 added value-only elided-owner propagation; its parent already returned None for elided keyed owners in the inverse graph. fcbadd07475 changes the forward route to an explicit owner-unresolved answer but retains the same key-side rejection. c520ffdb9b6 is a crate extraction, not an origin. The original on-demand get_definition boundary 18378088552 is 9337 lines after allowed exclusions. Do not count a newly moved or improved owner helper as introduction of an older missing-map-key case.
```

</details>

<details><summary>74: issue:3070 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:2`; original status `confirmed`, disposition `qualifying-candidate`.

Source attempt reason (verbatim):

```text
Unsafe indexed prior-write helper 7b4c2c67bd6 predates the recursion. e55ed6f4a8a newly handles subscript_expression receiver values and calls the helper recursively through the exact current RHS; this is the exposing runtime introduction. All prerequisites are visible and the 428-line diff passes the remaining rules.
```

</details>

<details><summary>75: issue:415 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:3`; original status `rejected`, disposition `oversize-original-introduction`.

Source attempt reason (verbatim):

```text
First stable marker is the real static-property forward lookup bug fixed by bc1a218d0f6. Blame points to e14fdac82d2, but that is a mechanical module split (24383 changed lines), not the origin. Following resolve_php into the original file reaches 18378088552, which adds the rejecting local-variable guard and lacks static-property routing; it is 9337 changed lines after allowed exclusions, above 4000. The second marker is an already-working inverse/alias control, not a distinct qualifying bug.
```

</details>

<details><summary>76: commit:5a61f8026268fd3e852cbad36039e5b8c0310ddd — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:4`; original status `rejected`, disposition `preflagged-and-unconfirmed-earlier-origin`.

Source attempt reason (verbatim):

```text
The named SHA 5a61f8026268 is not the complete fix: 729d746d54a adds the selected external inventory bridge and regression. 5a61f8026268 explicitly says in its introduction message that activated model probes retain UnsupportedHierarchyTraversal even for explicitly imported Serializable and that hierarchy closure remains unfinished. Its added plan repeats that boundary. It also says explicit-import behavior already failed before its qualified-parent registration change. Thus neither a newly undisclosed failure at that boundary nor an earlier exact introducing commit is established; do not relabel an acknowledged incomplete checkpoint.
```

</details>

<details><summary>77: commit:6074c794e6bd5c4eae8f160e4c938c42f645963e — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:5`; original status `confirmed`, disposition `qualifying-candidate`.

Source attempt reason (verbatim):

```text
The moved read-side helper at adbc30517fb is not a valid origin. Original 8f0d2a75bad adds the terminal-code-occurrence expansion and simultaneously rewires real usage consumers from RustUsageIndex to the lazy walk. Saved #2315 evidence establishes material request timeout impact. Match only terminal-name candidate widening and semantic verification, not hypothetical scans in the new fix SQL or the separate ecosystem-prefetch marker.
```

</details>

<details><summary>78: issue:1956 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:6`; original status `rejected`, disposition `oversize-original-and-alternate-introductions`.

Source attempt reason (verbatim):

```text
The value-position Call-row omission begins with the Ruby structural adapter in 35fb6a871555, whose complete diff is 5748 lines after allowed exclusions. d7285eea718 introduces the statement-only bare-call helper and the alternate false-call-on-bound-statement defect, but its complete diff is 4215 lines, also over 4000. 2e45de5d00d is only extraction of existing broken code, even though its 3480-line diff fits. All 17 stable-order marker descriptions were inspected: missing-value/argument/condition/top-level forms belong to the original omission, bound-statement misclassification to the later helper, and the remaining controls are added with the fix. No later policy false-clean endpoint may replace the pre-existing structural omission as its first origin.
```

</details>

<details><summary>79: commit:86e7440a87d35df723597ead9de254089156cb46 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:7`; original status `rejected`, disposition `no-qualifying-runtime-defect`.

Source attempt reason (verbatim):

```text
The two tests are ordinary behavior tests added with 86e7440a87d3: selected-directory roots and an empty-workspace fallback. Original 7af8d1ac531 watches the whole project recursively; the later change narrows resource scope but neither marker nor fixing history demonstrates a wrong result, crash/hang, leak, security issue, or measured material failure caused by that broad scope. Do not invent a resource-limit incident or reinterpret the new fallback control as a historical bug.
```

</details>

<details><summary>80: commit:28f0b8ec05a48f7589863afa8cb8c2ab24a4b464 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:8`; original status `rejected`, disposition `no-fixing-regression-and-preflagged-helper`.

Source attempt reason (verbatim):

```text
28f0b8ec05a4 is a test/plan-only commit adding an ignored regression; it is not a fix and is not an introduction of runtime code. The pinned current tree still ignores the test as a known #3775 defect. Tracing runtime code finds d31de9e1c8c, whose message AND added rust_unwrap_operand documentation expressly name custom same-name methods being treated as unwraps. The later receiver consumer 5d7e0f4c2b70 exposes the existing helper limitation for chained receiver calls, but no actual fixing commit was established for this marked ignored test. Do not publish the test-only SHA as either fix or intro.
```

</details>

<details><summary>81: commit:5f5b353ae5244a484528396232c8bb085d473213 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:9`; original status `rejected`, disposition `unconfirmed-source-snapshot-prerequisite`.

Source attempt reason (verbatim):

```text
The Windows line-recovery fix is real, but exact first runtime attribution remains uncertain. Blame reaches 7e3aab9b5c2, which moves an already byte-only helper from broad_symbol.rs; d049842cc0ff is the earlier endpoint boundary. The cited regression supplies byte offsets source.len()+offset, artificially outside the source, rather than actual LF offsets in CRLF source. This proves fallback behavior but not the runtime source/range mismatch prerequisite or which change first produces it. A June endpoint or July helper move cannot be confirmed as introduction of the later persisted-snapshot mismatch from this evidence.
```

</details>

<details><summary>82: issue:12922 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:10`; original status `rejected`, disposition `test-only-and-postcutoff`.

Source attempt reason (verbatim):

```text
d9b5d4fc8f8 repairs PathBuf::to_string_lossy usage only inside #[cfg(test)] fixtures. Its message explicitly states production already normalizes these paths in RustAnalyzer::acquire_configuration_inputs. The incorrect fixture keys were added by 1b2a66201197 on November 13, after cutoff. No production defect is attributable to this marker.
```

</details>

<details><summary>83: issue:10863 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:11`; original status `confirmed`, disposition `qualifying-candidate`.

Source attempt reason (verbatim):

```text
The November test/fix date is not a cutoff failure. d5c9845b7488 newly proves local struct expressions non-dropping using written impl_item declarations only; an item macro that expands to impl Drop is invisible to that proof. The exact S {} local/temporary form is pinned by the later macro-drop fixture. Narrow to local struct-expression absence proof, not later overwrite logic or cross-file absence, which the intro comments already discuss.
```

</details>

<details><summary>84: issue:11358 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-073-084/attempts.jsonl:12`; original status `rejected`, disposition `policy-scope-enhancement-not-factual-runtime-bug`.

Source attempt reason (verbatim):

```text
Actual documentation facts are introduced by 09eb9574ea54, not original python_rule_facts file creation 734f0093ad39. But the issue explicitly says missing-docstring findings are accurate, and the repair explicitly preserves CodeQL-parity documentation outcome and unchanged old-policy answers, adding test_scope/entry_point fields for downstream policy choices. The desired default slopcop exclusion is in external bifrost-packs/service policy, not demonstrated as a defect in this repository at the fact introduction. The new golden itself still emits outcome=issue for test declarations and filters only via the new fields. Do not force accurate raw facts plus later policy preferences into a wrong-result corpus bug.
```

</details>

<details><summary>85: issue:953 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:1`; original status `rejected`, disposition `size`.

Source attempt reason (verbatim):

```text
The named d5026b930 change optimizes the bounded Python dead-code target set and adds preservation/control tests; it is not the introducing missing-edge change. Tracing the previously repaired returned-member/keyword and namespace behavior through 7cac14b6 reaches original inverted Python usage-graph handler d0a2ca5f1710ef30806a2f762d5c97fab821cf70. That multi-language graph introduction is 5,953 changed lines after allowed exclusions, above 4,000. Do not choose the later split or performance optimization as a smaller intro. Both stable markers were checked; no qualifying alternative attributed.
```

</details>

<details><summary>86: issue:3134 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:2`; original status `rejected`, disposition `uncertain_attribution_runtime`.

Source attempt reason (verbatim):

```text
First stable member is the every_receiver_class_needs_a_dispatch_declaration helper regression. The defective count omission predates helper extraction dd9df356 and the complete-set amendment 9d635b519: original feedback 3b42f755 hardcodes exhaustive=true over only successful member declarations. However an intro-era receiver union with an absent member also emits an absent-member finding and sets the surviving hint proof Unproven; I did not establish an independently user-visible wrong result from that isolated flag, rather than merely a later strengthened invariant. Later markers include controls/conditional-tier additions, the separate resolved-method-return gap and a descriptor invocation fixed separately; exact introducing changes for those runtime witnesses were not confirmed within the bounded trace. Reject uncertainty; do not turn the helper extraction or test-bearing commit into an intro.
```

</details>

<details><summary>87: commit:bb45edd4de962f4fc229aa15caae3941edd9e250 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:3`; original status `rejected`, disposition `size`.

Source attempt reason (verbatim):

```text
Named bb45edd4 is a release selector-registration fix, not the introduction of these debug duplicate-name panics. Actual duplicate-binding exposure is new named row-selector paths in 8f11273e451dd41ba0b3fff1e73fbf41e57d3158 (4,907 changed lines after fixtures exclusion). Second stable marker, duplicate typestate events, traces by blame to original 3fdaa719695... event-ID selector paths, 78,710 nonfixture/nonlock lines. Both exceed 4,000. Later registration helpers and language-selection additions are not the earliest faulty path. Correct fix is 1eddfcd10, which adds both marked tests and validates duplicate IDs before selector construction.
```

</details>

<details><summary>88: issue:2601 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:4`; original status `rejected`, disposition `uncertain_attribution`.

Source attempt reason (verbatim):

```text
The marked test-bearing ef0ad16ce is explicitly test-only: its message says both reported Python spellings already bind after 10563175c, and keeps completion differences out of the claim. The corrective production boundary 10563175c newly adds resolver-owned imported-call proof, admits single-segment owners and changes model applicability. I did not establish an earlier source commit at which the old conservative external-call shape guard became defective under the shipped CPython summary contract; naming the fix or an old generic guard would not meet attribution/prerequisite rules. The pack-off marker is a negative control, not a distinct defect. Reject bounded uncertainty, not a claim that the fixed binding bug was unreal.
```

</details>

<details><summary>89: commit:874e8edbc950bd6ac4bc93913a632ce9697c7c5a — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:5`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
Actual value-class adaptation introduction omits the dispatch receiver from carrier classification. The later member-this regression and fix directly exercise the omitted path; see candidate and numbered proof.
```

</details>

<details><summary>90: issue:1893 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:6`; original status `rejected`, disposition `size`.

Source attempt reason (verbatim):

```text
Stable Java positive/negative and Kotlin pack markers pin the new consumer added by 1e9e3bec; activation support was older (31a39a1d900424110186a4cb3ac92a490b25d26b: 6,517 changed lines). The fourth marker pins a distinct trace-selection error corrected by b1b7cda1: finish_boundary left an indexed external answer as Rejected. Blame locates its original finish_boundary/external-row construction in d338f34f8e43f92e04232ee98ffbb15e4455d283 (14,001 changed lines), not 1e9e3bec, which only adds pack evidence to an already-existing source-jar route. No suitably narrow original introduction qualified; do not substitute the small corrective consumer commit.
```

</details>

<details><summary>91: issue:12792 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:7`; original status `rejected`, disposition `uncertain_runtime`.

Source attempt reason (verbatim):

```text
Traced actual receipt/channel history rather than rejecting by the November fix date. Execution marking exists in 8bdf6fee/5cf975419 and all-channel replay lands in pre-cutoff 61d43fc7. QueryReadReceipt is moved into common analysis in post-cutoff 19559d39; its message explicitly says no behavior change, so that is not an intro. f3bb9816 pins capture/replay execution-count equality and separates completion append from channel recording. I did not prove a qualifying intro-era user/client wrong result from a one-to-two positive execution-count change (boundedness already false), nor close the earlier receipt producer path. Later verdict/store-identity witnesses involve additional November behavior. Reject attribution/runtime uncertainty, not automatically post-cutoff.
```

</details>

<details><summary>92: issue:12249 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:8`; original status `rejected`, disposition `preflagged`.

Source attempt reason (verbatim):

```text
Actual producer introduction 24bd9be1e9d7fe2dd5cca7548e7a09a9a9750979 (September 9, 3,281 changed lines) explicitly says unqualified trait spellings remain typed incomplete and nothing is inferred from spelling. The marked November golden case proves the newly added bare Copy/Clone/Drop prelude authority implemented by 9cb5fb5c and narrowed by dad61927. This is the same limitation already named in the introducing message, so rule 5 excludes it; the November marker/fix dates alone are not the reason.
```

</details>

<details><summary>93: commit:ed722fa56803e3b783889067c3407372f4e401fc — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:9`; original status `rejected`, disposition `preflagged`.

Source attempt reason (verbatim):

```text
Named ed722fa5 is an explicit provisional test-only checkpoint, not a production fix. First stable test keeps impl/nested/attribute/import expansions open and is a conservative control. Second function-only test is the fail-before witness; actual repair is 5526963e, which adds a new matched-expansion ClosedMacroMemberSurface proof. Earlier 619a6c1b and 1a788389 explicitly retain UnexpandedItemMacro frontiers/member branches for still-unpersisted/unknown invocations, including the conservative surface the correction refines. I did not find a later unflagged introducing change; do not blame October language-test moves 1d2e3535/66182991. Exclude this previously stated member-uncertainty limitation under rule 5 rather than count the test checkpoint as introduction.
```

</details>

<details><summary>94: commit:db4a2c332dd24439eb197d33d717dbdc1df9bc99 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:10`; original status `rejected`, disposition `test_only`.

Source attempt reason (verbatim):

```text
All three markers belong to the standalone bifrost_reference_differential experiment runner, not product configuration/query/analysis behavior. db4a2c33 repairs its fingerprinted-pack equality check to allow legitimate extras. Blame locates equality in 8ec3835f948c1268a25040717dac40528bccd6e0; the fix message identifies later #2887 host-artifact activation as the exposure prerequisite, so the old check alone is not a sound runtime intro. Excluded as test/benchmark harness behavior under rule 4; did not borrow the unrelated activation feature as a product bug.
```

</details>

<details><summary>95: issue:8304 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:11`; original status `rejected`, disposition `size`.

Source attempt reason (verbatim):

```text
Canonical group is owned by first issue #8304; other #9050/#10002 references in its one compound marker are metadata, not extra weighted draws. The #8304 fix 881f0509 adds an advisory warning while retaining intentional opt-in behavior. The actual env-controlled early return is newly introduced by 31a39a1d900424110186a4cb3ac92a490b25d26b (6,517 changed lines after allowed exclusions), above 4,000. Do not attribute it to the small warning addition or later unrelated Rust-model/failed-file diagnostic components. They were not selected as substitute defects.
```

</details>

<details><summary>96: issue:8310 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-085-096/attempts.jsonl:12`; original status `rejected`, disposition `uncertain_findability`.

Source attempt reason (verbatim):

```text
438cc777 introduces the registry/helper but no query user surface; 1674bb68311e4791a1c51f451b9e2a86abd792ed adds the exposing configuration-facts query and reads the explicitly finite five-format registry. The #8310 correction has two stages: 958a3576 reports HOCON as a registered-but-unadapted incomplete format; 86533301 later adds real HOCON ingestion. I confirmed the pre-fix omission but did not establish that HOCON was part of the intro-era repository contract rather than a deliberately unregistered format: files outside the registry are explicitly not configuration even after the fix. Do not select old registry construction before a query exists or import later HOCON/Akka support as an intro prerequisite. Reject bounded findability/intent uncertainty.
```

</details>

<details><summary>97: issue:1867 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:1`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
Confirmed first fully qualifying defect in fixed member order; see candidate and qualification evidence.
```

Final parent-acceptance.json reason (verbatim):

```text
Lead ruling: SPEC4 not product behavior, checker-only PHP census oracle with no established product analyzer misbinding. Attribution trace retained.
```

</details>

<details><summary>98: commit:18a38bca1916c70744007806da28b3e3777c64f9 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:2`; original status `rejected`, disposition `not_findable_external`.

Source attempt reason (verbatim):

```text
The defect is a notify 9 FSEvents stop-before-CFRunLoop startup race inside an unvendored dependency. The per-path calls predate the notify 6->9 pin db37ea2; the pin is the observed activation boundary, but its diff/repository cannot establish the external implementation race. Do not attribute it to the older watcher helper.
```

</details>

<details><summary>99: commit:1c12dfc6a0341ab4911e08d97abde920bac48bed — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:3`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
Confirmed first fully qualifying defect in fixed member order; see candidate and qualification evidence.
```

</details>

<details><summary>100: issue:4408 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:4`; original status `rejected`, disposition `not_findable_external`.

Source attempt reason (verbatim):

```text
First marker is normal clone-with census membership, while the originating defect is external PHP grammar lack of PHP 8.5 clone-with syntax. The recorded issue and fix explicitly require published brokk-tree-sitter-php 0.24.4. The earlier 0.24.3 pin changes no clone grammar. No confirmed repo-local first defective runtime boundary for this grammar defect; do not substitute the new structured clone reader added with its repair.
```

</details>

<details><summary>101: commit:d0ce89824e2792b3faa4967315ad2e0252fd4312 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:5`; original status `rejected`, disposition `performance_only`.

Source attempt reason (verbatim):

```text
Marker and actual fix describe rebuilding/reprojecting an unchanged compatible host analyzer on warm policy requests. The repair avoids repeated identity resolution/Git status but records no wrong result, leak, hang or other nontrivial runtime defect. Named d0ce is an unrelated merge, not an introduction. Intro attribution stopped on SPEC rule 4; traced_intros intentionally empty.
```

</details>

<details><summary>102: issue:11370 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:6`; original status `rejected`, disposition `attribution_uncertain`.

Source attempt reason (verbatim):

```text
The fix honestly reports previously unanalyzed ERB templates. Blame follows seed enumeration through a50da/fc73 code-motion roots to older generic structural search (3ab6) and typed set composition (bdaf); these are not proven first defective Ruby/ERB runtime changes. The original structural search commit lacks Ruby adapters, and no qualifying activation was confirmed. Reject attribution uncertainty rather than label a longstanding generic helper as intro.
```

</details>

<details><summary>103: issue:7573 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:7`; original status `rejected`, disposition `performance_only`.

Source attempt reason (verbatim):

```text
Fixed SQL bypasses the SQLite prepared-statement cache. Actual fix says SQL text and behavior are unchanged and measures compilation CPU work; no wrong result/resource-exhaustion runtime failure is pinned. SPEC rule 4 performance trivia rejection; no intro attribution needed, traced_intros intentionally empty.
```

</details>

<details><summary>104: issue:1762 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:8`; original status `rejected`, disposition `attribution_uncertain`.

Source attempt reason (verbatim):

```text
The marker pins module-head over-follow. Trace reaches old export-index representation and the June25 forward walker, but that walker tries exact lookup before exported names; the known child module can therefore win. Later export-first resolution may activate the old helper. Do not force 547164b2 or the later code moves as first defective runtime change. Saved possible record is not a confirmed candidate.
```

</details>

<details><summary>105: commit:c5663ff4a73360ddfc182f34ebe05e283150573c — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:9`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
Confirmed first fully qualifying defect in fixed member order; see candidate and qualification evidence.
```

Final parent-acceptance.json reason (verbatim):

```text
Lead ruling: SPEC4 not product behavior, validator CLI grading/sampling identity only, no analyzer or product-ID change. Attribution trace retained.
```

</details>

<details><summary>106: issue:5331 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:10`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
Confirmed first fully qualifying defect in fixed member order; see candidate and qualification evidence.
```

</details>

<details><summary>107: issue:13312 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:11`; original status `rejected`, disposition `post_cutoff`.

Source attempt reason (verbatim):

```text
The marker expressly names the #13069 regression. Fix 23e856 and source boundary 756bc20 show dropped UnknownInteger literal seeds in the November 15 change, after the November 1 cutoff. Earlier scalar literal helpers are not valid introductions.
```

</details>

<details><summary>108: issue:8264 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-097-108/attempts.jsonl:12`; original status `rejected`, disposition `oversize`.

Source attempt reason (verbatim):

```text
Actual producer precedence bug is already in initial Go API-pack producer 1ec35bb: go_builtin_type wins before package type_ids. Its whole diff is +7359 -186 after golden/fixture/lock exclusions (7545 >4000). d72a only centralizes the same universe-name predicate, so it is not a smaller qualifying intro; do not blame that refactor.
```

</details>

<details><summary>109: issue:188 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:1`; original status `rejected`, disposition `size_limit`.

Source attempt reason (verbatim):

```text
The actual benchmark assertion originates in 4dc88359. Its parent diff is +5192 -4; excluding Cargo.lock (+64 -4) and the 979-line checked-in baseline still leaves 4149 changed lines, above 4000. No artificial narrowing to runner.rs. The fix explicitly pins degraded compact-symbol responses; runtime prerequisite timing was not used to rescue this oversized boundary.
```

</details>

<details><summary>110: issue:1828 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:2`; original status `rejected`, disposition `uncertain_runtime_activation`.

Source attempt reason (verbatim):

```text
Traced the fixed canonicalization-failure/ambiguity conflation past 2f1583a3, cef57b5c and ba6e7344 to the a669cc76 lexical resolver. That early helper is initially an inverse owner resolver; this group's first marker is a public forward-definition call to an unindexed member alias. The trace did not establish the first runtime forward activation or prove that this exact public status regression already exists at the helper boundary. Reject rather than label an old helper birth as the runtime introduction. All later members are either the same unindexed-alias defect or indexed/builtin/ambiguity controls.
```

</details>

<details><summary>111: issue:12761 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:3`; original status `confirmed`, disposition `confirmed_candidate`.

Source attempt reason (verbatim):

```text
Exact canonical marker requalified against its fixing change, first defective runtime change, introduction prerequisites, size, message/comments, and master ancestry; see candidate and evidence.
```

</details>

<details><summary>112: commit:31913fee42a05ec61aadd1f3ca0a76ba1fed51f8 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:4`; original status `rejected`, disposition `uncertain_first_defective_change`.

Source attempt reason (verbatim):

```text
Member order respected: anonymous-aggregate extraction first, bare-macro-guard implication second, g_autoptr recovery third. The first marker is introduced with new anonymous receiver support in 31913fee; the earlier 9cce90a8 tag-scope predicate cannot be assigned this receiver defect before that receiver support exists. The second marker traces through dc1b8c5a Boolean implication, but the parent already fails to prove a bare X guard from X && Y; this is an existing omission, not a regression newly introduced by dc1b8c5a. Earlier d9dfbb4f guards and 1c570a35 opaque-expression handling are recorded, without claiming their helper births prove the exact runtime boundary. The third is new syntax recovery whose first defective runtime boundary was not established within the bounded trace. No forced candidate.
```

</details>

<details><summary>113: issue:8599 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:5`; original status `rejected`, disposition `size_limit`.

Source attempt reason (verbatim):

```text
First this::method-reference marker fixes the qualifier-is_some exceptional gap added in a4625155, the all-language CFG/ICFG rollout. That full runtime introduction is far above 4000 lines, with tens of thousands of source lines outside fixtures. The second type-qualified marker concerns the same qualifier gap family from that rollout; do not substitute a later move into the JVM crate or policy activation as its origin.
```

</details>

<details><summary>114: issue:2854 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:6`; original status `rejected`, disposition `size_and_activation_uncertainty`.

Source attempt reason (verbatim):

```text
The actual fix permits complete empty authored summaries in the validator. That rejection traces to 63feeb6a's activation-neutral pack payload implementation, before CSMI export exists. The group specifically pins CSMI complete-empty export; d883a143 first adds that export path and its own diff exceeds 4000 lines even after conformance fixture exclusions. No confirmed smaller change establishes a newly defective complete-empty export contract. The old validator birth is not automatically a valid CSMI runtime introduction.
```

</details>

<details><summary>115: commit:c854959b282948f64831b685e350126573cdf496 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:7`; original status `rejected`, disposition `size_limit`.

Source attempt reason (verbatim):

```text
The environment clearing and allowlist that omit LOCALAPPDATA/GOCACHE are actually introduced in 1ec35bbc (Go exact API packs). Its +7361 -188 diff exceeds 4000 lines after fixture exclusions. c854959b is the small fix, not the defective introduction; a later crate move is not valid attribution.
```

</details>

<details><summary>116: issue:2213 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:8`; original status `rejected`, disposition `uncertain_first_defective_change`.

Source attempt reason (verbatim):

```text
The actual fix adds ordinary using-directive owner resolution for nested classes. 0a2aeacd is an inherited-base recovery helper addition; 6b509560 adds a class-owned-alias-specific path. Neither is shown to remove a working ordinary nested-class path, and the imported-class omission can predate both. Do not attribute the defect to those helper births. The first defective runtime lookup/filtering boundary remains unresolved; no candidate.
```

</details>

<details><summary>117: commit:093b17cd76edaf2c67b3189fd0045262d0b4608b — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:9`; original status `confirmed`, disposition `confirmed_candidate`.

Source attempt reason (verbatim):

```text
Exact canonical marker requalified against its fixing change, first defective runtime change, introduction prerequisites, size, message/comments, and master ancestry; see candidate and evidence.
```

</details>

<details><summary>118: issue:1518 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:10`; original status `rejected`, disposition `intro_contract_not_findable`.

Source attempt reason (verbatim):

```text
Stable member order: the public-entry-point test is new coverage, not an identified runtime repair; the path validator test likewise does not establish a prior defective change. The first actual runtime repair is the insertion/deletion-only endpoint pairing defect. It traces to aee20dbd side-specific patch effects. However that intro message and its design text explicitly define preimage membership by old-line overlap and postimage membership by new-line overlap (the exact behavior later changed), so an intro-only reviewer lacks the later endpoint-pair invariant. Reject this contract/expectation shift rather than presenting the later invariant as unflagged at intro. Remaining members in the group are controls/coverage or the same endpoint-pair defect.
```

</details>

<details><summary>119: commit:12f759b2964cdae96832e59b7fe5dafc87a92832 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:11`; original status `confirmed`, disposition `confirmed_candidate`.

Source attempt reason (verbatim):

```text
Exact canonical marker requalified against its fixing change, first defective runtime change, introduction prerequisites, size, message/comments, and master ancestry; see candidate and evidence.
```

</details>

<details><summary>120: commit:e83f35411767aaffedbf1da5092d49e2d4a64b1d — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-109-120/attempts.jsonl:12`; original status `confirmed`, disposition `confirmed_candidate`.

Source attempt reason (verbatim):

```text
Exact canonical marker requalified against its fixing change, first defective runtime change, introduction prerequisites, size, message/comments, and master ancestry; see candidate and evidence.
```

</details>

<details><summary>121: commit:69e0507ebf4be4487d9b93020101e470a1b01541 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:1`; original status `rejected`, disposition `post-cutoff`.

Source attempt reason (verbatim):

```text
The first two markers pin bounded membership certification added by80d3/8bd7 and refactored in e0b090. All are Nov6–7. Third marker is prefix replay of depth answers introduced853aed on Nov7. Exact fixes69e050 are post-cutoff too; older generic caller-inventory helpers are not these exposing changes.
```

</details>

<details><summary>122: commit:23f2f89b2092448d5d0ae742a5603ed7da352cd8 — qualified_unselected_spread</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:2`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
First fully qualifying marker in stable member order; all SPEC checks recorded.
```

Final spread-check.json reason (verbatim):

```text
Parent decision: third case in inverse-usage-candidate-file-routing after77/117; SPEC8 cap. Confirmed trace remains eligible, unselected for spread only.
```

</details>

<details><summary>123: issue:7183 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:3`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
First fully qualifying marker in stable member order; all SPEC checks recorded.
```

</details>

<details><summary>124: commit:e3a7cdf04a43b8401f419a39ad6fab4a3743e868 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:4`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
First fully qualifying marker in stable member order; all SPEC checks recorded.
```

</details>

<details><summary>125: commit:e91fd3a11f33b67f6d4bd7fd23976c6766b2e0e8 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:5`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
First fully qualifying marker in stable member order; all SPEC checks recorded.
```

Final parent-acceptance.json reason (verbatim):

```text
Not product correctness at proposed intro: Kotlin empty-file omission prevents cache retention, but current proof/graph remains available; reject performance-only exposure. See independent audit.
```

</details>

<details><summary>126: commit:654654dd4755dcc5cba9317a045b9d17551942a3 — not_qualified</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:6`; original status `rejected`, disposition `preflagged`.

Source attempt reason (verbatim):

```text
The rejected forwards-only owner behavior originates in2f34151. Its commit message explicitly states that forwards-only evidence remains ambiguous, the limitation this group’s fix removes. All four markers concern that same unique-direct-forward owner family; arity checks are controls. Cannot use an older parent helper or ignore the explicit limitation to qualify the case.
```

</details>

<details><summary>127: commit:36fa96bd75ca0de467734e93e780bcc177f2a461 — selected</summary>

Source: `/mnt/optane/review-arms-bifrost-20261116/corpus-build/groups-121-132/attempts.jsonl:7`; original status `confirmed`, disposition `candidate`.

Source attempt reason (verbatim):

```text
First fully qualifying marker in stable member order; all SPEC checks recorded.
```

</details>

## Provenance and limits

Inputs: source `groups-*/attempts.jsonl`, superseded issue/comment attempt records, `parent-acceptance.json`, `spread-check.json`, frozen `corpus/corpus.jsonl`, and the explicitly requested original registered ordinary/Mjolnir comparison metadata. Input SHA256s are recorded in `yield-report.json`. Census inputs/outputs, outcome labels and model findings/results were not accessed. No builds, tests, reviews, source traces, sessions, config changes, selection edits or commits were performed. Parent owns synthesis and final freeze.

UTC correction, 2026-11-17T03:41:57.338958+00:00: parent updated only the ordinary comparison after same-seed redraw (8230 eligible); source yield decisions and bug/census data unchanged. Earlier bytes are preserved under utc-cutoff-correction/pre-correction/.
