# Split machines from runtimes in the user configuration

This ExecPlan is a living document. The sections `Progress`, `Surprises & Discoveries`, `Decision Log`, and `Outcomes & Retrospective` must be kept up to date as work proceeds. It is maintained in accordance with `.agents/PLANS.md` at the repository root.

## Purpose / Big Picture

Today a user describes where sessions run with one `[targets.<id>]` table per entry, and each entry fuses two different things: the machine (this computer, an SSH host, or an EC2 launch template) and the runtime on that machine (a bare checkout, Podman, Docker, or Apple `container`). Because of that fusion, three targets that all run on this computer (`localhost`, `podman`, `docker`) look like three unrelated places, and per-machine settings such as the mbx build cache directory and size limit have to be repeated on every container target that shares a host. The Settings screen shows the same fused list under one page called "Machines and Runtimes".

After this change the configuration file and the Settings screen have two separate collections. `[machines.<id>]` lists hosts: the implicit `local` machine, SSH hosts, and EC2 launch templates. Each machine owns its SSH connection details, its remote workspace directory for bare runtimes, and its mbx build cache settings. `[targets.<id>]` lists runtimes: `bare`, `podman`, `docker`, or `apple-container`, each naming the machine it runs on (`machine = "local"` is the default and is omitted from the file). A user can see it working by opening Settings in the terminal dashboard: the page list shows "Machines" and "Runtimes" separately, the Machines page has one `local` entry with the build cache settings, and every runtime entry shows a "Machine" field. Existing configuration files load unchanged and are rewritten in the new shape on the next save. The controller, worker, database, and web viewer keep using the resolved per-target type they use today, so no session behavior changes except that local Podman and local Docker now share one build cache inspection.

## Progress

- [x] (2026-09-17) Milestone 1: stored configuration shape (`Machine`, `StoredTarget`, `StoredConfig`), lossless conversion both ways, legacy migration on load, version 11, validation, and round-trip tests in `mj-core`.
- [ ] Milestone 2: shared cache host keyed by machine (`CacheHost::Local | Ssh`), `preview_build_cache` taking a machine, and the controller's mbx tests.
- [ ] Milestone 3: Settings screen pages "Machines" and "Runtimes", machine choice on runtimes, build cache preview under machines, path resolution under machines, detection inserting runtimes on `local`, and TUI behavior tests.
- [ ] Milestone 4: documentation, the launch-template script, and the end-to-end fixtures write the new shape; full workspace validation.

Known transient state: between Milestone 1 and Milestone 3, four Settings-screen tests in `mj-tui` fail (`setup_adds_a_remote_runtime_and_reports_invalid_fields_without_losing_the_draft`, `the_automatic_download_policy_shows_what_it_does_to_this_image`, `remote_path_apply_preserves_failed_and_newer_drafts`, `the_build_cache_page_shows_the_values_its_host_resolves_for_blank_fields`). The Settings draft is literally `serde_json::to_value(&Config)`, so the moment the stored shape changes the screen's schema is out of date, and the schema is Milestone 3's subject. Every other package is green at each milestone, and Milestone 3 restores the whole suite.

## Surprises & Discoveries

- Observation: an error raised inside `TryFrom<StoredConfig> for Config` reaches the caller as a plain string, because the TOML and JSON deserializers turn it into their own error through `Error::custom(message.to_string())`, which keeps no `anyhow` source chain. `.context(...)` therefore disappeared and only the outermost sentence survived.
  Evidence: the first run of `raw_ssh_permissions_are_required_and_podman_rejects_them` failed with `parse Mjolnir config /tmp/.../config.toml: target "builder" (ssh-bare)`, with serde's own `missing field 'permissions'` missing from the text. Every message these conversions raise now embeds the detail directly (`anyhow!("target {id:?} ({kind}): {error}")`).

- Observation: a `Config` assembled in memory carries no `machines`, but the same configuration read back from a file does, because saving names the hosts its targets share. Equality between "the config I built" and "the config that was loaded" is therefore not automatic.
  Evidence: `raw_ssh_permissions_are_required_and_podman_rejects_them` and `controller::resume::tests::a_failed_raw_conversion_keeps_the_checkout_and_its_previous_checkpoint` both compared a hand-built config against a loaded one and failed with `machines: {}` on one side and `machines: {"builder": Ssh { .. }}` on the other. Both tests now state the machine that saving names.

## Decision Log

- Decision: Keep `mj_core::config::TargetTemplate` (the eight-variant fused enum) as the in-memory, resolved type that every consumer already matches on, and introduce the machine/runtime split only in the stored form (the TOML file and the JSON draft the Settings screen edits).
  Rationale: an inventory found over a hundred match sites on the fused enum across mj-core, mj-controller, mj-cli, mj-client, and mj-tui, plus the SQLite `session_targets.kind` CHECK constraint and the web viewer's `kind` strings, all of which need the resolved host-plus-runtime pair. Rewriting them would be pure churn with no behavior change. The user-visible goals (one host concept, machine-owned mbx settings, separate pages) are all at the stored layer.
  Date/Author: 2026-09-17, Fable.

- Decision: The in-memory `Config` gains `machines: BTreeMap<String, Machine>`, and each in-memory target still carries copies of its machine's SSH connection and build cache. On save, a target is matched to its machine by equality of those copies; on load, the copies are filled from the machine. The machine entry is the source of truth for build cache settings.
  Rationale: this avoids adding a `machine` field to the in-memory enum, which would break every construction site, while still keeping one place (the machine) that owns host settings.
  Date/Author: 2026-09-17, Fable.

- Decision: When a version 10 file has several container targets on the same machine with different non-default `build_cache` settings, the first one in target-id order wins and a `tracing::warn!` names the ignored targets. Loading does not fail.
  Rationale: failing the load would lock the user out of the Settings screen they need to fix it. The user's stated intent is that these settings were never meaningfully separable.
  Date/Author: 2026-09-17, Fable.

- Decision: Legacy target kinds (`local-bare`, `local-podman`, `local-docker`, `apple-container` under `targets` without a `machine`, `ssh-bare`, `ssh-podman`, `ssh-docker`, `aws-ec2`) are accepted only when the file's `version` is 10 or lower. A version 11 file that uses them is rejected with a message naming the target and the new spelling.
  Rationale: accepting both spellings forever would leave two ways to write the same thing; the version number already exists to gate this.
  Date/Author: 2026-09-17, Fable.

- Decision: `reject_non_bare_permissions` is not deleted; the same rule moved into `interpret_target` in `mj-core/src/config.rs`, which rejects a `permissions` key on any kind other than `bare` or the legacy `ssh-bare`.
  Rationale: the plan said to delete the check because resolution would enforce the same thing, but it would not: `StoredTarget::Podman` flattens `ContainerTemplate`, and serde's flatten silently absorbs an unmatched key instead of reporting it, so `permissions` on a container runtime would be dropped without a word. Doing the check on the parsed table also covers the JSON draft the Settings screen sends, which the old raw-TOML check never saw. The message is now "target {id:?} sets `permissions`, which only applies to a bare runtime"; the existing test asserts on that wording instead of "only valid for ssh-bare".
  Date/Author: 2026-09-17, Fable.

- Decision: an SSH machine is matched to a target by its `SshConnection` alone, not by the connection plus the workspace directory as the plan said. When a bare SSH target's `workspace_prefix` differs from the machine's, the machine's wins and a `tracing::warn!` names both.
  Rationale: validation rejects two machines with the same connection, so matching on the pair would synthesize a second machine for the same host and then fail its own validation on save. At most one machine can hold a connection, so the connection identifies it.
  Date/Author: 2026-09-17, Fable.

- Decision: a `[targets.<id>]` table is deserialized into a raw `serde_json::Value` (`TargetEntry::Raw`) and interpreted in `TryFrom<StoredConfig> for Config`, rather than being dispatched during deserialization.
  Rationale: whether an old `kind` is accepted, and whether `apple-container` means the fused kind or the stored one, both depend on the file's `version`, which is a sibling key the field's own `Deserialize` cannot see. Interpreting after the whole document is read is the only place both are known.
  Date/Author: 2026-09-17, Fable.

- Decision: migration and saving are the same code path. `stored_target` takes a fused `TargetTemplate` and returns the stored runtime plus the machine it needs, and loading a pre-version-11 file calls it on each legacy table.
  Rationale: a legacy `[targets.<id>]` table parses into exactly the fused enum, so migrating a file and saving an in-memory config are the same operation. One implementation cannot drift from the other.
  Date/Author: 2026-09-17, Fable.

- Decision: Config version 11 is a breaking file change for older builds, handled by the existing `newer_version` guard which makes an older build refuse to load or overwrite the file with a clear "Update Mjolnir" message.
  Rationale: this is the established mechanism in `Config::load_from` and `Config::ensure_writable`; no new mechanism is needed.
  Date/Author: 2026-09-17, Fable.

## Outcomes & Retrospective

Milestone 1 is done. `mj-core` now reads and writes `[machines.<id>]` and `[targets.<id>]` separately, migrates every pre-version-11 file on load, and refuses the old fused kinds in a version 11 file. Nothing outside `mj-core/src/config` changed shape: `TargetTemplate` still has its eight variants and every consumer still matches on it. The remaining milestones move the cache host, the Settings screen, and the documentation onto the new shape.

## Context and Orientation

Mjolnir is a Rust workspace. The crate `mj-core` holds shared types including the user configuration in `mj-core/src/config.rs` and `mj-core/src/config/targets.rs`. The crate `mj-controller` runs sessions and owns the mbx build cache logic in `mj-controller/src/controller/mbx.rs` and `mj-controller/src/controller/cache_host.rs`. The crate `mj-tui` is the terminal dashboard, whose Settings screen is `mj-tui/src/setup.rs` with field labels, defaults, and choices in `mj-tui/src/setup/schema.rs`. The crate `mj-cli` is the `mj` binary; its dashboard I/O layer in `mj-cli/src/dashboard/io/spawn.rs` runs background work for the TUI, including setup detection and the build cache preview.

Package names differ from directory names: `cargo test -p brokk-mj-core`, `-p brokk-mj-controller`, `-p brokk-mj-tui`, `-p brokk-mjolnir` (the CLI). Run every `cargo test` outside the sandbox with elevated permissions; the suite uses loopback sockets.

The configuration file is TOML at the path returned by `mj_core::config::config_path()`. `Config` (`mj-core/src/config.rs`, around line 333) derives `Serialize`/`Deserialize` with `deny_unknown_fields`, carries `pub version: u32` checked against `CONFIG_VERSION` (currently 10, line 320), and has `pub targets: BTreeMap<String, TargetTemplate>`. `Config::load_from` (line 525) parses the file, refuses files written by a newer version, bumps versions 1 through 9 to the current version in memory, and validates. `Config::save_to_locked` (line 641) validates and writes `toml::to_string_pretty(self)`. The Settings screen edits a `serde_json::Value` produced by `serde_json::to_value(&config)` and turns it back with `serde_json::from_value` (`config_from_draft`, `mj-tui/src/setup.rs` around line 210), so whatever `Config` serializes to is exactly what the Settings screen shows.

`TargetTemplate` in `mj-core/src/config/targets.rs` (line 351) is an internally tagged enum (`kind = ...`) with eight variants: `LocalBare`, `LocalPodman { container }`, `LocalDocker { container }`, `AppleContainer { container }`, `AwsEc2 { aws_profile, region, launch_template, launch_template_version, ssh_user, address_source, identity_file, ssh_args }`, `SshBare { ssh, permissions, workspace_prefix }`, `SshPodman { ssh, container }`, `SshDocker { ssh, container }`. `ContainerTemplate` (line 222) holds `image`, `pull_policy`, `platform`, `cpus`, `memory`, `environment`, `workspace_storage`, and `build_cache: Option<TargetBuildCache>`. `SshConnection` (line 320) holds `host`, `user`, `identity_file`, `extra_args`. `TargetBuildCache` (line 139) holds `enabled`, `directory`, `max_size`, all optional. `AwsEc2` always runs a bare harness over SSH on the instance; it never runs a container (`mj-controller/src/controller/backend.rs` `backend_target`, around line 509, produces an `AwsTemplate` with no container).

"mbx" is the Rust build cache tool. `mj-controller/src/controller/mbx.rs` resolves, per container target, where the cache lives on the target's host. `CacheHost` in `cache_host.rs` names that host; today it has five variants (`LocalPodman`, `LocalDocker`, `Apple`, `SshPodman(SshTarget)`, `SshDocker(SshTarget)`) and `key()` returns a different string for local Podman and local Docker, so the same physical machine is inspected and cached twice. `preview_build_cache` (`mbx.rs` line 147) is what the Settings screen calls to show a blank build cache field's resolved value; it receives only a `mj_core::config::TargetTemplate`.

`Config::with_local_targets` (`mj-core/src/config.rs` line 490) adds implicit `localhost`, `podman`, `docker`, and `apple-container` targets in memory, and `Config::update` strips unmodified implicit ones before saving. `Config::setup_additions` (line 399) computes collision-free additions for detection. `unique_config_id` and `validate_id` live in `mj-core/src/config/loading.rs` (ids are 1 to 64 ASCII letters, digits, `.`, `-`, `_`). `reject_non_bare_permissions` in `loading.rs` (line 14) is a raw-TOML check that `permissions` appears only on `ssh-bare` targets.

## Plan of Work

### Milestone 1: stored configuration shape in `mj-core`

Create `mj-core/src/config/machines.rs` and declare it from `mj-core/src/config.rs` alongside `targets`. Define the machine type:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "kebab-case")]
    pub enum Machine {
        Local {
            #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_target_build_cache")]
            build_cache: Option<TargetBuildCache>,
        },
        Ssh {
            #[serde(flatten)]
            ssh: SshConnection,
            /// Where bare runtimes on this machine keep their workspaces.
            #[serde(default = "default_named_machine_prefix")]
            workspace_prefix: PathBuf,
            #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "deserialize_target_build_cache")]
            build_cache: Option<TargetBuildCache>,
        },
        AwsEc2 {
            // exactly the eight fields TargetTemplate::AwsEc2 has today, same serde attributes
        },
    }

Reuse `deserialize_target_build_cache` and `default_named_machine_prefix` from `targets.rs` (make them `pub(super)`). The local machine's id is always `local`; validation rejects a `Machine::Local` under any other id and a non-local machine under the id `local`. `Machine::validate(&self, id)` reuses `SshConnection::validate`, the workspace prefix safety check now in `TargetTemplate::validate` for `SshBare` (move it to a shared `validate_workspace_prefix` function), the AWS field checks now in `TargetTemplate::validate` for `AwsEc2`, and `TargetBuildCache::validate`. Two SSH machines with equal `SshConnection` values are rejected ("machines {a:?} and {b:?} describe the same host").

Define the stored runtime type in the same file:

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "kebab-case")]
    pub enum StoredTarget {
        Bare {
            #[serde(default = "local_machine_id", skip_serializing_if = "is_local_machine_id")]
            machine: String,
            /// Only meaningful on an SSH machine; the local machine always uses configured approvals.
            #[serde(default, skip_serializing_if = "Option::is_none")]
            permissions: Option<PermissionMode>,
        },
        Podman { machine, #[serde(flatten)] container: ContainerTemplate },
        Docker { machine, #[serde(flatten)] container: ContainerTemplate },
        AppleContainer { machine, #[serde(flatten)] container: ContainerTemplate },
    }

`ContainerTemplate` keeps its `build_cache` field and serde attributes so legacy files still read it, but a stored target whose container has a non-default `build_cache` is rejected on load with "target {id:?} sets build_cache, which now belongs to [machines.{machine}]", and conversion to the stored form always clears it.

Define `StoredConfig`, a private struct with every field of `Config` in the same order and with the same serde attributes, except `targets: BTreeMap<String, StoredTargetOrLegacy>` and a new `machines: BTreeMap<String, Machine>` placed before `targets` with `#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]`. Put `#[serde(into = "StoredConfig", try_from = "StoredConfig")]` on `Config` and remove the direct derives' effect (keep `Serialize`/`Deserialize` derives on `Config`; serde's `into`/`try_from` attributes route them). Add `pub machines: BTreeMap<String, Machine>` to `Config` (after `bundles`, before `targets`) and to `Config::default`.

Legacy reading. `StoredTargetOrLegacy` is a custom-deserialized wrapper: deserialize the table into `serde_json::Value` (check that `mj-core` already depends on `serde_json`; it does for state, confirm in `mj-core/Cargo.toml`), read its `kind` string, and dispatch: `bare | podman | docker | apple-container` go to `StoredTarget`, the eight legacy kinds go to `TargetTemplate`, anything else is an error listing the four valid kinds. Note that `apple-container` is a valid kind in both shapes; treat it as legacy only when the table has no `machine` key and the config version is 10 or lower, otherwise as stored. Serialization of the wrapper always writes the `StoredTarget` form.

`TryFrom<StoredConfig> for Config` does, in order: refuse legacy kinds when `version == 11` (message: "target {id:?} uses the old kind {kind:?}; write kind = {new:?} and machine = {machine:?}"); migrate legacy targets into machines and stored targets (rules below); then resolve every stored target against `machines` into a `TargetTemplate` (rules below); then set `version = CONFIG_VERSION` when it was 1 through 10 (this replaces the `1..=9` bump in `load_from`, which must be removed so the two rules do not disagree); then return the `Config`. Validation stays in `Config::validate`, which `load_from` calls afterwards.

Migration rules for one legacy target `id`:

- `LocalBare` becomes `Bare { machine: "local", permissions: None }`.
- `LocalPodman | LocalDocker | AppleContainer { container }` become the matching stored kind on `local`, with `container.build_cache` taken out and folded into `machines.local.build_cache` (create `Machine::Local` if absent). If `machines.local.build_cache` is already non-default and differs, keep the existing one and `tracing::warn!` naming both target ids.
- `SshBare { ssh, permissions, workspace_prefix }`, `SshPodman { ssh, container }`, `SshDocker { ssh, container }` look for an existing `Machine::Ssh` with an equal `ssh`; if none, create one whose id is `ssh.host` when `validate_id` accepts it and it is unused, otherwise `unique_config_id(&machines, id)`. The `SshBare` prefix sets the machine's `workspace_prefix` (first one wins, warn on a differing later one); container build caches fold in as for local. The target becomes `Bare { machine, permissions: Some(permissions) }` or the container kind with `machine`.
- `AwsEc2 { .. }` becomes `Machine::AwsEc2` with the same fields under `unique_config_id(&machines, id)` (normally `id` itself, because machine and target ids are separate namespaces) and the target becomes `Bare { machine, permissions: None }`.

Resolution rules for a stored target into `TargetTemplate` (fail with a clear message when the machine is missing: "target {id:?} names machine {machine:?}, which is not defined"; `local` is always defined even when `[machines.local]` is absent):

- `Bare` on `local` becomes `LocalBare` (`permissions` must be `None`, else "target {id:?} sets permissions, which only applies to a bare runtime on an SSH machine"). `Bare` on `Ssh` becomes `SshBare { ssh, permissions: permissions.unwrap_or(PermissionMode::Guardian), workspace_prefix }`. `Bare` on `AwsEc2` becomes `AwsEc2 { .. }` with the machine's fields.
- `Podman | Docker` on `local` become `LocalPodman | LocalDocker` with `container.build_cache = machine.build_cache.clone()`; on `Ssh` they become `SshPodman | SshDocker { ssh, container }` likewise; on `AwsEc2` they are rejected ("target {id:?}: an EC2 machine runs a bare harness only").
- `AppleContainer` on `local` becomes `AppleContainer { container }` with the build cache copied for consistency; on any other machine it is rejected ("Apple container runs only on this machine").

`From<Config> for StoredConfig` (used by `Serialize`) does the reverse: for each target, find its machine: local kinds match `local`; `Ssh*` kinds match the `Machine::Ssh` with equal `ssh` (and, for `SshBare`, equal `workspace_prefix`); `AwsEc2` matches the `Machine::AwsEc2` with equal fields. If there is no match, synthesize a machine using the same id rule as migration so a `Config` built in memory (the CLI wizard's `build_config` in `mj-controller/src/setup.rs` still constructs `TargetTemplate` values) saves correctly. Clear `container.build_cache` on the stored form. Copy `machines` through, adding the `local` entry only when it has a non-default build cache so a default file stays minimal. The conversion must be lossless: add a property-style test that for a `Config` built from every variant with every optional field set, `Config -> StoredConfig -> Config` is equal, and that `toml::to_string_pretty` then `toml::from_str` round-trips.

Update `Config::validate` to validate `machines` before `targets` and to call the new stored-form checks. Remove `reject_non_bare_permissions` from `load_from` and delete it from `loading.rs` together with its test, since the resolution rules now enforce the same thing with a message that names the new shape. Update `setup_additions` to also compute machine additions (dedupe by `Machine` equality, base id = discovered id), and `state.rs` config-change detection (`mj-core/src/state.rs` around line 1605) needs no change because it compares resolved targets.

Bump `CONFIG_VERSION` to 11. Update the comment above the version bump in `load_from` to say version 11 splits machines from runtimes. Update `mj-core/src/config/tests.rs`: every fixture that writes `[targets.x] kind = "ssh-podman"` style TOML either sets `version = 10` (to test migration) or is rewritten in the new shape. Add tests: a version 10 file with `localhost`, `podman` (with `build_cache`), `docker`, `builder` (`ssh-bare`), `builder-podman` (`ssh-podman`, same host) and `aws` loads into a `Config` whose `machines` are `local` (with that build cache), `builder.example.com` (or the host string used), and `aws`, whose resolved targets equal the old fused values, and which saves as version 11 in the new shape (assert on the TOML text); a version 11 file with a legacy kind is rejected; a stored target naming a missing machine is rejected; `permissions` on a local bare target is rejected; a container on an EC2 machine is rejected; two SSH machines with the same connection are rejected.

### Milestone 2: one cache host per machine in `mj-controller`

Replace `CacheHost`'s five variants with `Local` and `Ssh(SshTarget)`. `for_target` maps `LocalPodman | LocalDocker | AppleContainer` to `Local` and `SshPodman | SshDocker { ssh, .. }` to `Ssh(ssh.clone())`; `key()` returns `"local"` or `format!("ssh:{}", ssh.destination)`; `ssh()` and `command()` collapse accordingly. `supported_host` in `mbx.rs` keeps excluding Apple container, bare, and EC2 targets.

Change `preview_build_cache` to take `(machine: &mj_core::config::Machine, global: &BuildCacheConfig, executor)` and to build the `CacheHost` directly from the machine: `Machine::Local` gives `CacheHost::Local`, `Machine::Ssh { ssh, .. }` gives `CacheHost::Ssh(SshTarget::from(ssh))`, `Machine::AwsEc2` returns `Ok(None)` (no shared cache). The settings are the machine's `build_cache.clone().unwrap_or_default()`. It no longer calls `backend_target`. Update the `DashboardAction::PreviewBuildCache` payload in `mj-tui/src/lib.rs` and its handler in `mj-cli/src/dashboard/actions.rs` and `spawn.rs` to carry a `Box<Machine>` instead of a target.

Update the mbx tests in `mbx.rs` (around lines 859 to 1033) and any `cache_host` tests so that two local container targets resolve through one host key, proving the shared inspection: a test that resolves a `LocalPodman` and then a `LocalDocker` target with the same settings must show one call to the host inspection (the existing fake executor records commands; assert the second resolve is served from `RESOLUTIONS`).

### Milestone 3: Settings screen

In `mj-tui/src/setup/schema.rs`:

- `defaults`: the root object gains `"machines":{}` before `"targets"`. Add `"machines" if path.len() == 2 => machine_defaults(kind)` where `kind` defaults to `"ssh"` for a newly added entry (the local machine already exists in the draft; see below): `local` has `build_cache: {enabled: null, directory: null, max_size: null}`; `ssh` has `host: "", user: null, identity_file: null, extra_args: [], workspace_prefix: ".local/share/hel/workspaces", build_cache: {...}`; `aws-ec2` has the eight AWS fields as `target_defaults` gives them today. `target_defaults` becomes: `kind` defaults to `"podman"`, every entry has `machine: "local"`, `bare` adds `permissions: null`, container kinds add the container fields without `build_cache`. Remove the `ssh-`/`aws-ec2` branches from `target_defaults`.
- When the dialog opens (`SetupDialog::new` or wherever the draft is first expanded), insert `machines.local` as `{"kind":"local","build_cache":{...}}` if absent so the Machines page always shows the local machine. `config_from_draft` must drop a `local` machine whose build cache is all null before deserializing, or the stored form must tolerate it (it does: `Machine::Local` with `build_cache: None` is valid; keep it simple and let it serialize, but `From<Config> for StoredConfig` omits it when default, so it never reaches the file).
- `label`: `"machines" => "Machines"`, `"targets" => "Runtimes"`, `"machine" => "Machine"`, `"workspace_prefix" => "Workspace directory for bare runtimes"`.
- `choices`: `"kind"` under `machines` gives `["local", "ssh", "aws-ec2"]` (but the `local` entry's kind is not editable; hide `kind` in `visible_keys` for `machines.local`); `"kind"` under `targets` gives `["bare", "podman", "docker", "apple-container"]`; `"machine"` gives `["local"]` plus every key of `draft["machines"]` except `local` duplicates. `"permissions"` unchanged.
- `choice_label`: `"local" => "This machine"`, `"ssh" => "SSH host"`, `"aws-ec2" => "Amazon EC2"`, `"bare" => "Bare checkout"`, `"podman" => "Podman"`, `"docker" => "Docker"`, `"apple-container" => "Apple container"`; for a `machine` value show the id as is. Remove the eight old kind labels.
- `null_label`: the `build_cache` and `platform` arms move to `["machines", _, "build_cache", _]` and, for platform, read the runtime's `machine` and show the arch only when it is `local`. `["targets", _, "permissions"]` shows `Ask for approvals` when the runtime's machine is `local`? No: on `local` the field is not applicable; hide `permissions` in `visible_keys` when `machine == "local"`, and on an SSH machine a null shows `Ask for approvals` (the `Guardian` default). `["machines", _, "user" | "identity_file" | "aws_profile" | "launch_template_version"]` take over the old target arms.
- `help`: `"machines"` says "Add an SSH host or an EC2 launch template. This machine is always listed as local. Build cache settings live here because every runtime on a machine shares them."; `"targets"` says "Add a runtime and choose the machine it runs on, or use Detect runtimes to find this machine's usable container engines."; `"machine"` says "Which machine this runtime runs on."
- `path_kind`: `["machines", _, "identity_file"] => Local`; `["machines", _, "workspace_prefix"] | ["machines", _, "build_cache", "directory"] | ["targets", _, "workspace_storage", "root"] => Target`.

In `mj-tui/src/setup.rs`:

- `collection()` treats `machines` as a collection too. Removing `machines.local` is refused with a notice ("This machine is always available").
- `resolve_path_action`: for `PathKind::Target`, find the machine: under `machines` it is `draft["machines"][id]`; under `targets` it is `draft["machines"][draft["targets"][id]["machine"]]` or the implicit local. Deserialize it into `Machine` and send it in `DashboardAction::ResolveSetupPath` in place of the target; update the handler in `mj-cli` to run the remote `echo ~` (or whatever it does today) against the machine's SSH connection or locally.
- `build_cache_page`: the path is now `["machines", machine_id, "build_cache"]`; the key is `{"machine": draft["machines"][machine_id], "global": draft["build_cache"]}`; `preview_build_cache_action` parses `Machine` and `BuildCacheConfig`.
- Detection (`setup_discovered`, `DetectScope::Runtimes`) is unchanged in logic; because additions serialize through the stored form, they land as `{"kind":"podman","machine":"local",...}` automatically. Add the assertion to the existing detection test.
- `visible_keys`: hide `kind` for `machines.local`; hide `permissions` for a runtime whose `machine` is `local`.
- Validation notice on save: `config_from_draft` errors already surface through `set_failure_notice`; make sure the new messages (missing machine, container on EC2) show there. Add a test that a draft with a runtime naming a missing machine produces that notice on Save.

Update `mj-tui/src/setup/tests.rs`: fixtures that build targets in JSON use the new shape; the "every blank setting names its effect" test adds one machine of each kind and one runtime of each kind and keeps its assertion; the page-action test adds the Machines page (Add and Remove, no Detect); add `a_runtime_chooses_its_machine_from_the_configured_machines` asserting the `machine` combo lists `local` plus the draft's machine ids.

### Milestone 4: documentation, scripts, fixtures, full validation

Rewrite the target sections of `docs/src/content/docs/configuration.md` and `docs/src/content/docs/targets.md` around two headings, `## Machines [machines.<id>]` and `## Runtimes [targets.<id>]`, with one example per machine kind and per runtime kind and a note that files written before version 11 are converted on the next save. Update the examples in `docs/src/content/docs/ssh.md`, `aws.md`, `docker.md`, `podman.md`, `apple-container.md`, `custom-images.md`, `containers.md`, `profiles.md`, and the duplicated `docs/AWS.md`, `docs/SSH.md`, `docs/DOCKER.md`, `docs/PODMAN.md`, `docs/README.md` (check each for `[targets.` and `kind = "` first; leave files that have neither). `scripts/update-runson-launch-template.sh` (around line 200) and its test `scripts/test-update-runson-launch-template.sh` write a `[machines.aws-runson]` table with `kind = "aws-ec2"` and a `[targets.aws-runson]` table with `kind = "bare"` and `machine = "aws-runson"`. The end-to-end fixtures `tests/e2e/tui_components_tmux.py`, `tests/e2e/session_move.py`, `tests/e2e/reliability_lab.py`, and `tests/e2e/ssh_docker_acceptance.py` write `kind = "bare"` (and `kind = "docker"` with a `[machines.<host>]` table for the SSH Docker acceptance file); check what `version` line they write and set it to 11.

Update the human-readable plan summary in `mj-controller/src/setup.rs` (around lines 1368 to 1432) only if it prints kind names; the wizard itself keeps building `TargetTemplate` values.

## Concrete Steps

All commands run from `/home/jonathan/Projects/hel`, outside the sandbox with elevated permissions.

After each milestone:

    cargo fmt --all
    cargo clippy --all-targets -- -D warnings
    cargo test -p brokk-mj-core -p brokk-mj-controller -p brokk-mj-tui -p brokk-mjolnir

After Milestone 4, the full suite:

    cargo test

Commit each milestone on the current branch with a plain-language message, staging only the files changed, ending with:

    Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
    Claude-Session: https://claude.ai/code/session_018gjirMqFEBqL79SgkaBMwa

## Validation and Acceptance

Behavior a person can verify:

1. Take a version 10 configuration such as

       version = 10

       [targets.localhost]
       kind = "local-bare"

       [targets.podman]
       kind = "local-podman"
       image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"

       [targets.podman.build_cache]
       max_size = "50GiB"

       [targets.builder]
       kind = "ssh-podman"
       host = "builder.example.com"
       image = "ghcr.io/brokkai/mjolnir/agent-dev:latest"

   Load it with `Config::load_from` in a test (or point `MJ_CONFIG_DIR` at a scratch directory and run `mj setup --help`, which loads the config) and save it. The saved file starts with `version = 11`, has `[machines.local]` with `build_cache.max_size = "50GiB"`, `[machines."builder.example.com"]` with `kind = "ssh"` and `host = "builder.example.com"`, `[targets.localhost]` with `kind = "bare"` and no `machine` line, `[targets.podman]` with `kind = "podman"` and no build cache, and `[targets.builder]` with `kind = "podman"` and `machine = "builder.example.com"`.

2. A file with `version = 11` and `kind = "local-podman"` fails to load with a message that names the target and says to write `kind = "podman"`.

3. In the terminal dashboard, open Settings. The list shows "Machines" and "Runtimes" as separate pages. Machines contains `local` and any SSH or EC2 machines. Opening `local` shows the build cache settings, which resolve to the host's values as they did before under the old page. Opening a runtime shows a "Machine" field whose choices are `local` plus every machine id.

4. `cargo test -p brokk-mj-controller mbx` shows the new test proving that resolving a local Podman and then a local Docker target inspects the host once.

5. `cargo test` for the whole workspace and `cargo clippy --all-targets -- -D warnings` pass.

## Idempotence and Recovery

Loading a version 11 file is a no-op conversion, so repeated loads and saves are stable. A version 10 file is converted only in memory until the next save, so a user who downgrades before saving loses nothing. After a save the file is version 11; an older Mjolnir refuses it with "was written by a newer Mjolnir (config version 11 ...)" rather than corrupting it. Tests that write configuration files use isolated `MJ_CONFIG_DIR` and `MJ_DATA_DIR` directories, as the existing tests do; do not run any of this against the live store.

## Artifacts and Notes

(fill in with the round-trip test output and the saved-TOML excerpt from acceptance step 1 when done)

## Interfaces and Dependencies

In `mj-core/src/config/machines.rs` (new), public:

    pub const LOCAL_MACHINE_ID: &str = "local";
    pub enum Machine { Local { build_cache }, Ssh { ssh, workspace_prefix, build_cache }, AwsEc2 { .. } }
    pub enum StoredTarget { Bare { machine, permissions }, Podman { machine, container }, Docker { machine, container }, AppleContainer { machine, container } }
    impl Machine { pub fn kind_name(&self) -> &'static str; pub fn build_cache(&self) -> Option<&TargetBuildCache>; }
    pub fn resolve_target(id: &str, target: &StoredTarget, machines: &BTreeMap<String, Machine>) -> anyhow::Result<TargetTemplate>;
    pub fn stored_target(id: &str, target: &TargetTemplate, machines: &mut BTreeMap<String, Machine>) -> (StoredTarget /* machine id inserted into `machines` when new */);

In `mj-core/src/config.rs`:

    pub const CONFIG_VERSION: u32 = 11;
    pub struct Config { ..., pub machines: BTreeMap<String, Machine>, pub targets: BTreeMap<String, TargetTemplate> }   // serde via StoredConfig
    pub use machines::{LOCAL_MACHINE_ID, Machine, StoredTarget};

In `mj-controller/src/controller/cache_host.rs`:

    pub(super) enum CacheHost { Local, Ssh(SshTarget) }

In `mj-controller/src/controller/mbx.rs`:

    pub fn preview_build_cache(machine: &mj_core::config::Machine, global: &BuildCacheConfig, executor: &impl CommandExecutor) -> Result<Option<BuildCachePreview>>;

In `mj-tui/src/lib.rs`, `DashboardAction::PreviewBuildCache { generation, key, machine: Box<Machine>, global }` and `DashboardAction::ResolveSetupPath { .., machine: Box<Machine> }`.

No new crates and no new external dependencies. `mj-core` uses `serde_json` for the legacy-kind dispatch; confirm it is already a dependency of `mj-core` before relying on it, and if it is not, implement the dispatch with a small hand-written `Deserialize` that buffers into `serde::__private::de::Content`-free form (a `BTreeMap<String, toml::Value>` is not acceptable because the Settings screen feeds JSON), so adding `serde_json` to `mj-core` is the fallback.
