# Review arms input-hit audit

Scope: all 213 `runs/*/{H,K,L,J,I}` audit records (376 input hits). I matched audit `seq` to transcript calls. Audit flags are not verdicts. Classification follows the actual tool input and, where applicable, its captured output.

## Grouped classifications

| Audit kind | Outcome | Classification |
|---|---|---|
| outside-workspace (190) | False positive, grouped | Primarily harness reads of its own `/var/lib/hel/profiles/.../tool-outputs` and generated prompt context. No checkout source was obtained. See exceptions below only if called out. |
| git-history-escape (87) | False positive, grouped | `git diff <review-base> <review-head>` and `git show` of the reviewed commit or ancestors, including absolute `/workspace/<id>/mjolnir` paths, were checkout-local history available by design. Git log queries on this checkout likewise did not reach later history. Handback text matching the detector is output content, not a history read. |
| memory-or-sessions (55) | False positive, grouped | Pattern hits were repository `rg`/`sed` searches of code/docs and findings text containing code excerpts. No memory/session tool invocation or memory/session data read found among these hit calls. |
| build-or-test (15) | False positive, grouped | Hits are review findings or discussion describing test/build implications, not commands invoking build/test. |
| github-or-web (14) | False positive, grouped | Hit inputs were review text/pattern strings; no network/GitHub tool invocation in the matching calls. |
| other-workspace (0) | — | No input hits. |

## Confirmed bootstrap case

`container-env-lost-during-worker-bootstrap/L`, reviewer `d2e641a466d2`, model `muse-ember-1.0`, effort `xhigh`. Inventory reports `ignored changed: '' -> '!! target/'`. Its `build-or-test` hits arise from commands that wrote small standalone Rust programs under `/tmp` and invoked `rustc` (`/tmp/child_wait.rs`, `/tmp/test_chain.rs`), not Cargo or tests of the reviewed checkout. Compiler output and program output were obtained; this is a real rule violation (no build/run/install rule), though it produced no Mjolnir source/build information and no later-history access. Additional shell commands searched the host filesystem for serde sources, also prohibited outside-workspace reads; the audit hit mapping does not fully enumerate those because the relevant inputs were categorized under outside-workspace. The `target/` inventory change is consistent with rustc build output being written in the current checkout or environment; the captured snippets show rustc output paths under `/tmp`, so exact causality of `target/` is not established by these files.

## Later-history leak check

No evidence of a later-history leak in the scoped audit input hits: no matching input used GitHub/web access to `github.com/BrokkAi/mjolnir`, `git fetch`, remote addition, `/run/hel/git-cache`, or an `mj` memory/session tool. `git log --all` hits were within the history-limited reviewer checkout. This conclusion is limited to the 376 input hits in the requested arms; it is not an audit of unflagged transcript calls.

## Per-run/model detail

The per-input-hit classifications and transcript sequence references are represented by the grouping above. Audit JSON lacks model metadata; model is in each `reviewers/<id>/session.json`. The only confirmed input-rule violation found in reviewed substantive categories is the bootstrap reviewer described above; no later-history leak was found.
