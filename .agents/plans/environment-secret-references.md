# Keep credentials out of config.toml with environment references

This ExecPlan follows `.agents/PLANS.md` and records how a profile's or container's `environment` table can name a secret instead of holding it.

## Purpose / Big Picture

A Mjolnir profile that authenticates with an API key had to write that key as a plain string under `[profiles.<id>.environment]` in `config.toml`. That file is copied into every isolated `--instance`, pasted into bug reports, and read by agents diagnosing a setup, so the key travelled with it. After this change an entry can be written as `{ from_secret = "NAME" }`, read from a `secrets.toml` beside `config.toml`, or `{ from_env = "NAME" }`, read from the environment of the process that loads the configuration. Every consumer still sees plain strings; saving a setting writes the reference back, never the value; and `mj doctor` points out credentials still written as plain text and a secrets file other users can read.

## Progress

- [x] (2026-09-28) `Environment` type in `mj-core/src/config/secrets.rs`: entries as written plus resolved values, `Deref` to the resolved map, serialization of the entries as written. Reference forms `from_env` and `from_secret`; a resolver that `Config::load_from` installs for the parse, reading `secrets.toml` beside the configuration file.
- [x] (2026-09-28) `HarnessProfile.environment` and `ContainerTemplate.environment` use it; consumers that mutate a copy read `.resolved().clone()`.
- [x] (2026-09-28) `mj doctor`: `secrets.file` reports the secrets file's permissions; `profiles.<id>.secrets` and `targets.<id>.secrets` warn about plain-text values under credential-like names with the exact reference to write.
- [x] (2026-09-28) Documentation: configuration reference gains a Secrets section; the targets page points at it.
- [x] (2026-09-28) Tests: reference parsing, serialization, missing-secret errors, malformed references, secrets file shape and mode, doctor checks, and a load-save round trip that keeps the reference spelling.

## Surprises & Discoveries

The in-place save compares each key's file spelling with the value this build would write and rewrites any key that differs, so resolving references at load and serializing the resolved value would have written the secret into `config.toml` on the next settings save. The type therefore keeps the entries as written and serializes those.

A section whose every value was default is rewritten whole, with defaults, when one of its keys changes. This predates the change and is unrelated to references; the round-trip test uses a non-default `[notify]` table as the existing save test does.

The terminal settings editor works on a JSON projection of the configuration and treats `environment` entries as strings. A reference appears there as a nested object with one field and can be opened and edited as such.

## Decision Log

- Decision: resolve references while the configuration is read, through a resolver installed for the parse, rather than at each consumer. Rationale: about a hundred places construct or read profile environments; they keep working with plain strings, and one place decides where secrets come from. Date: 2026-09-28.
- Decision: the secrets file is a flat TOML table of strings beside `config.toml`. Rationale: each `--instance` has its own, copying a configuration never copies a secret, and the shape is too small to get wrong. Date: 2026-09-28.
- Decision: no configuration version bump. Rationale: plain strings keep working in every build, only a file that uses a reference needs this build, and a bump would make every older checkout sharing the same `config.toml` refuse it after the first save. Date: 2026-09-28.
- Decision: the doctor warns about a shared secrets file instead of the loader refusing it. Rationale: refusing would stop sessions over a permission bit; the warning names the `chmod` to run. Date: 2026-09-28.

## Context and Orientation

`mj-core/src/config.rs` holds `Config` and `Config::load_from`, which parses `config.toml` and validates it. `mj-core/src/config/harness.rs` holds `HarnessProfile`; `mj-core/src/config/targets.rs` holds `ContainerTemplate` and `TargetTemplate`. `mj-core/src/config/document.rs` edits the file in place on save. `mj-core/src/config/secrets.rs` is new. `mj-controller/src/doctor.rs` assembles `mj doctor` checks in `run_with_config_path`.

## Validation and Acceptance

`cargo test -p brokk-mj-core --lib` runs the parsing, serialization, secrets-file, and round-trip tests. `cargo test -p brokk-mj-controller --lib doctor` runs the doctor checks. With a profile whose `environment` names a secret, `mj doctor` shows the secrets file check and no plain-text warning; with the value written as a string, it shows the warning with the reference to write. A configuration that names a missing secret fails to load with the entry and the file named.

## Outcomes & Retrospective

Implemented and validated as recorded above. Existing configurations with literal values keep working unchanged; the doctor tells their owners what to move.
