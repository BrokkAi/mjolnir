# Provenance

This directory holds the study plan's "Bifrost today" census measurement:
today's Bifrost run on each census decisive fact's labelled query. Final
result: 57 of 57 rows, measured on 2026-11-20 and 2026-11-21.

- Produced by the "RQL census Bifrost measurements" helper session
  (bdbef0e9) on ec2-m8id16-c, in its bifrost-dev workspace at
  `.agents/scratch/rql-census-measure/`, and copied here unchanged.
- Bifrost commit 683ae81b01, built with the `measure` profile.
- `report/measurements.jsonl` sha256
  `570ef7cb1d9f841753c81960a7bbd33f1fe7a5d1103ffea4633e62ce4353ed84`,
  matching the producing session's own hash.
- `report/README.md` documents the setup, boundaries, counts and
  per-repo table. `judgments.json` holds the per-row judgments. `bin/`
  holds the runner scripts.
- The per-query raw JSON and watchdog reports are archived outside the
  repo, at `/mnt/optane/review-arms-pilot/bifrost-today/raw.zip`
  (sha256 `7a484253f8d758ddee65a6beb747b3ffeaef70df564e29c0e3956a5e69f06b1e`),
  with an unpacked copy beside it.

Summary, from the producing session:

- 53 queries ran and 4 timed out at the 1800 s query boundary. None
  were unmappable, hit query errors, failed indexing or were killed.
  Peak index RSS was 8.56 GiB.
- The decisive row was present in 15 of the 53 that ran, and the result
  was complete in 11. Every labelled query parsed, so the gaps are in
  the answers, not the grammar.
- Four results were complete without the decisive row (possible
  fail-opens). One is closure calls, fixed by #15034. The other three
  were routed for filing on 2026-11-21:
  - a `#[cfg(windows)]` caller missing from call-sites-to, with no
    inactive-cfg diagnostic;
  - two cases where `(declaration :name X)` doesn't match a Rust `const`
    item.
