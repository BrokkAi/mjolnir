//! Deterministic project-image recipes shared by the daemon and CLI output.

use std::collections::BTreeMap;

pub use crate::runtime_manifests::platform_arch;
use anyhow::{Result, ensure};
use base64::Engine;
use mj_core::config::HarnessKind;
use mj_core::harness_runtime::*;
use sha2::{Digest, Sha256};

/// Files mise reads at the repository root: the one list both the worker's
/// post-clone step (which checkouts have a recipe) and the image generator
/// (which files an image is built from) use. `go.mod` is deliberately absent:
/// it changes with every dependency update, which would rebuild the image.
/// Ordering is fixed by the map.
pub const RECIPE_FILES: &[&str] = &[
    "mise.toml",
    ".mise.toml",
    "mise/config.toml",
    ".config/mise.toml",
    ".tool-versions",
    "mise.lock",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".nvmrc",
    ".node-version",
    ".python-version",
    ".ruby-version",
    ".go-version",
];

/// Where a built image keeps the recipe files it was built from. The worker
/// compares a checkout's recipe with this copy to know the image ran the
/// recipe's postinstall hook.
pub const IMAGE_RECIPE_DIR: &str = "/opt/mj-image/recipe";

pub use crate::runtime_manifests::{MiseArtifact, pinned_mise};

#[derive(Debug, Clone, Default)]
pub struct ImageBase {
    /// Platform-specific, immutable reference (repository@sha256:digest).
    pub reference: String,
    pub user: String,
    pub workdir: String,
    pub baked_node: Option<String>,
    pub baked_harnesses: BTreeMap<String, String>,
}

pub fn recipe_hash(files: &BTreeMap<String, String>) -> Result<String> {
    Ok(mj_core::hex::lower_hex(Sha256::digest(serde_json::to_vec(
        files,
    )?)))
}

/// Directory of `exec` wrappers for every baked tool, after the mise shims on
/// `PATH`. A shim resolves versions from the working directory's mise
/// configuration and fails ("No version is set for shim") outside a checkout:
/// before the clone, in `/`, `/tmp`, or `/workspace/<id>`. Inside a checkout the
/// shims come first, so the checkout's own versions win; elsewhere the lookup
/// falls through to these wrappers, which run the version baked into the image.
/// The fall-through is mise's own: with `not_found_system_fallback = true` (the
/// default in the pinned mise 2026.11.9), a shim with no active project
/// `mise.toml` searches the remaining `PATH` entries after `/opt/mise/shims`. It
/// therefore reaches `/opt/mj-image/bin` before `/usr/bin`, and exits "No version
/// is set for shim" only when no later `PATH` entry provides the binary.
const IMAGE_FALLBACK_BIN_DIR: &str = "/opt/mj-image/bin";

/// Shell for the image build's install layer, run in the recipe directory right
/// after `mise install`. It writes one `exec` wrapper per executable in
/// `mise bin-paths`, earlier directories winning. Wrappers rather than symlinks
/// keep `argv[0]` and `$0`-relative lookups pointing at the real install
/// directory. It fails when `mise bin-paths` or a wrapper write fails.
fn fallback_wrappers_command(bin_dir: &str) -> String {
    format!(
        r##"rm -rf '{bin_dir}' && mkdir -p '{bin_dir}' && dirs=$(mise bin-paths) && printf '%s\n' "$dirs" | while IFS= read -r dir; do [ -d "$dir" ] || continue; for bin in "$dir"/*; do [ -f "$bin" ] && [ -x "$bin" ] || continue; name=${{bin##*/}}; [ -e '{bin_dir}'"/$name" ] && continue; escaped=$(printf '%s' "$bin" | sed "s/'/'\\\\''/g"); printf "#!/bin/sh\nexec '%s' \"\$@\"\n" "$escaped" > '{bin_dir}'"/$name" && chmod 0755 '{bin_dir}'"/$name" || exit 1; done; done"##
    )
}

/// The complete build input: no source checkout or build context is required.
pub fn containerfile(
    base: &ImageBase,
    platform: &str,
    mise: &MiseArtifact,
    files: &BTreeMap<String, String>,
    harnesses: &[HarnessKind],
) -> Result<String> {
    for value in [
        &base.reference,
        &base.user,
        &base.workdir,
        platform,
        &mise.url,
        &mise.version,
    ] {
        ensure!(
            !value.contains(['\r', '\n']),
            "image metadata contains a newline"
        );
    }
    ensure!(
        mise.sha256.len() == 64 && mise.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid mise SHA256"
    );
    ensure!(
        base.reference.contains("@sha256:"),
        "base image must be pinned by digest"
    );
    let recipe = recipe_hash(files)?;
    let (stage, copies, labels) = harness_stage(base, platform, harnesses)?;
    let mut text = stage;
    text.push_str(&format!(
        "FROM {}\nUSER root\nADD --checksum=sha256:{} {} /usr/local/bin/mise\nRUN chmod 0755 /usr/local/bin/mise\nENV MISE_DATA_DIR=/opt/mise \\\n    RUSTUP_HOME=/opt/rustup \\\n    CARGO_HOME=/opt/cargo \\\n    MISE_TRUSTED_CONFIG_PATHS={IMAGE_RECIPE_DIR} \\\n    MISE_IDIOMATIC_VERSION_FILE_ENABLE_TOOLS=rust,node,python,ruby,go \\\n    PATH=/opt/mise/shims:{IMAGE_FALLBACK_BIN_DIR}:$PATH\nWORKDIR {IMAGE_RECIPE_DIR}\n",
        base.reference, mise.sha256, mise.url
    ));
    for (path, contents) in files {
        ensure!(
            RECIPE_FILES.contains(&path.as_str()),
            "unsupported recipe file {path:?}"
        );
        emit_embedded_file(&mut text, path, contents.as_bytes(), "0644");
    }
    // Sessions on a non-root base run as that user and install recipe changes
    // into these directories. Chown in the install layer: a later layer would
    // copy every installed file up again.
    let user = base.user.split(':').next().unwrap_or_default();
    let chown = if user.is_empty() || user == "root" || user == "0" {
        String::new()
    } else {
        ensure!(
            base.user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte)),
            "unsupported base image user {:?}",
            base.user
        );
        format!(
            " && mkdir -p /opt/mise /opt/rustup /opt/cargo && chown -R {} /opt/mise /opt/rustup /opt/cargo",
            base.user
        )
    };
    text.push_str(&format!(
        "RUN mise bootstrap packages apply --yes && mise install && {}{chown}\n",
        fallback_wrappers_command(IMAGE_FALLBACK_BIN_DIR)
    ));
    text.push_str(&copies);
    text.push_str(&labels);
    text.push_str(&format!("LABEL dev.mj.image.recipe=\"{recipe}\" \\\n    dev.mj.image.mise={} \\\n    dev.mj.image.platform={}\n", serde_json::to_string(&mise.version)?, serde_json::to_string(platform)?));
    text.push_str(&format!(
        "USER {}\nWORKDIR {}\n",
        if base.user.is_empty() {
            "root"
        } else {
            &base.user
        },
        serde_json::to_string(if base.workdir.is_empty() {
            "/"
        } else {
            &base.workdir
        })?
    ));
    Ok(text)
}

pub fn image_tag(containerfile: &str) -> String {
    mj_core::hex::lower_hex(Sha256::digest(containerfile.as_bytes()))[..16].to_owned()
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::*;

    fn executable(path: &Path, text: &str) {
        std::fs::write(path, text).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Runs the exact install-layer snippet against a fake `mise` whose
    /// `bin-paths` prints `mise_output` and exits with `status`.
    fn run_snippet(root: &Path, mise_output: &str, status: i32) -> std::process::Output {
        let fake = root.join("fake");
        std::fs::create_dir_all(&fake).unwrap();
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = bin-paths ] || exit 64\ncat <<'EOF_PATHS'\n{mise_output}\nEOF_PATHS\nexit {status}\n"
        );
        executable(&fake.join("mise"), &script);
        let bin_dir = root.join("out bin");
        Command::new("/bin/sh")
            .arg("-c")
            .arg(fallback_wrappers_command(bin_dir.to_str().unwrap()))
            .env("PATH", format!("{}:/usr/bin:/bin", fake.display()))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    #[test]
    fn node_image_branch_uses_compatible_system_node_without_glibc() {
        let real_node =
            crate::program_path::find_program_on_path("node", std::env::var_os("PATH").as_deref())
                .unwrap();
        let base = ImageBase {
            reference: format!("example.invalid/base@sha256:{}", "a".repeat(64)),
            ..Default::default()
        };
        let artifact = crate::runtime_manifests::artifact("node", "linux-x86_64").unwrap();
        let interpreter = artifact.interpreter.unwrap();
        for dsh in [false, true] {
            let kinds = if dsh {
                vec![HarnessKind::Claude, HarnessKind::Dsh]
            } else {
                vec![HarnessKind::Claude]
            };
            let (stage, _, _) = harness_stage(&base, "linux/amd64", &kinds).unwrap();
            let command = stage
                .lines()
                .find(|line| line.starts_with("RUN ") && line.contains(BAKED_NODE_MANIFEST_FILE))
                .unwrap()
                .strip_prefix("RUN ")
                .unwrap();
            assert!(command.contains(&node_version_check_script(if dsh {
                DSH_ACP_MINIMUM_NODE_VERSION
            } else {
                NODE_MINIMUM_VERSION
            })));
            assert!(command.contains("tar -xzf"));
            for (version, compatible) in [("18.0.0", false), ("22.0.0", !dsh), ("24.9.0", true)] {
                let root = tempfile::tempdir().unwrap();
                let bin = root.path().join("bin");
                std::fs::create_dir(&bin).unwrap();
                executable(
                    &bin.join("node"),
                    &format!(
                        r#"#!/bin/sh
exec '{}' -e 'require("vm").runInNewContext(process.argv[1], {{process: {{versions: {{node: process.env.MJ_TEST_NODE_VERSION}}, exit: process.exit.bind(process)}}}})' "$2"
"#,
                        real_node.display()
                    ),
                );
                for name in ["npm", "npx"] {
                    executable(&bin.join(name), "#!/bin/sh\nexit 0\n");
                }
                let archive = root.path().join("archive.tar.gz");
                std::fs::write(&archive, "must be removed").unwrap();
                let store = root.path().join("store");
                let command = command
                    .replace(
                        &interpreter,
                        root.path().join("missing-loader").to_str().unwrap(),
                    )
                    .replace("/opt/mj-harnesses", store.to_str().unwrap())
                    .replace("/tmp/mj-node.tar.gz", archive.to_str().unwrap());
                let output = Command::new("/bin/sh")
                    .arg("-c")
                    .arg(command)
                    .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                    .env("MJ_TEST_NODE_VERSION", version)
                    .stdin(Stdio::null())
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.success(),
                    compatible,
                    "{version} dsh={dsh}: {output:?}"
                );
                assert!(!archive.exists());
                let installed =
                    store.join(format!("node/node-v{NODE_VERSION}-{}", artifact.target));
                assert!(installed.is_dir());
                assert!(!installed.join(BAKED_NODE_MANIFEST_FILE).exists());
                assert!(!installed.join("bin").exists());
                if !compatible {
                    assert!(
                        String::from_utf8_lossy(&output.stderr).contains("requires glibc loader")
                    );
                }
            }
        }
    }

    #[test]
    fn image_install_scripts_check_parses_npm_json_with_node() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("scripts.json");
        for (status, pending) in [
            (
                r#"[{"changes":[{"change":"pending","key":"first@1"},{"change":"allowed","key":"ok@1"}]}]"#,
                "first@1",
            ),
            (
                r#"{"allowScripts":[{"changes":[{"change":
                "pending","key":"second@1"}]}]}"#,
                "second@1",
            ),
            (
                r#"{"change":"pending","key":"irrelevant","allowScripts":[{"changes":[{"change":"pending"}]}]}"#,
                "",
            ),
        ] {
            std::fs::write(&file, status).unwrap();
            let output = Command::new("node")
                .arg("-e")
                .arg(INSTALL_SCRIPTS_CHECK)
                .arg(&file)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert_eq!(output.status.success(), pending.is_empty(), "{output:?}");
            assert_eq!(String::from_utf8(output.stderr).unwrap().trim(), pending);
        }
    }

    #[test]
    fn pin_changes_rebuild_only_changed_subtrees_after_mise_install() {
        let mut base = ImageBase {
            reference: format!("example.invalid/base@sha256:{}", "a".repeat(64)),
            ..Default::default()
        };
        let mise = pinned_mise("x86_64").unwrap();
        let kinds = [HarnessKind::Codex, HarnessKind::Grok];
        let initial = containerfile(&base, "linux/amd64", &mise, &BTreeMap::new(), &kinds).unwrap();
        assert!(initial.contains(" AS mj-harnesses"));
        assert!(initial.find("mise install").unwrap() < initial.find("COPY --from").unwrap());
        assert!(initial.contains("--cache /tmp/mj-npm-cache"));
        assert!(initial.contains("createZstdDecompress"));
        let final_stage = initial.rsplit_once("FROM ").unwrap().1;
        assert!(!final_stage.contains(".tar.gz"));
        assert!(!final_stage.contains(".zst"));
        assert!(!final_stage.contains("_cacache"));
        base.baked_node = Some(NODE_VERSION.into());
        for kind in kinds {
            base.baked_harnesses
                .insert(kind.id().into(), pin(kind).install_id.into());
        }
        let inherited =
            containerfile(&base, "linux/amd64", &mise, &BTreeMap::new(), &kinds).unwrap();
        assert!(!inherited.contains(" AS mj-harnesses"));
        assert!(inherited.contains("dev.mj.harnesses.codex="));
        assert_eq!(inherited.matches("LABEL dev.mj.harnesses.").count(), 1);
        base.baked_harnesses.insert("codex".into(), "stale".into());
        let partial = containerfile(&base, "linux/amd64", &mise, &BTreeMap::new(), &kinds).unwrap();
        assert!(partial.contains("COPY --from=mj-harnesses /opt/mj-harnesses/harnesses/codex/"));
        assert!(!partial.contains("COPY --from=mj-harnesses /opt/mj-harnesses/harnesses/grok/"));
        assert!(!partial.contains("COPY --from=mj-harnesses /opt/mj-harnesses/node/"));
        assert_ne!(image_tag(&inherited), image_tag(&partial));
        assert_eq!(
            initial,
            containerfile(
                &ImageBase {
                    baked_node: None,
                    baked_harnesses: BTreeMap::new(),
                    ..base.clone()
                },
                "linux/amd64",
                &mise,
                &BTreeMap::new(),
                &[HarnessKind::Grok, HarnessKind::Codex, HarnessKind::Codex]
            )
            .unwrap()
        );
    }

    #[test]
    fn generic_base_bakes_both_architectures_without_project_layers() {
        let base = ImageBase {
            reference: format!("example.invalid/base@sha256:{}", "a".repeat(64)),
            ..Default::default()
        };
        for platform in ["linux/amd64", "linux/arm64"] {
            let text = session_base_containerfile(&base, platform).unwrap();
            assert!(!text.contains(IMAGE_RECIPE_DIR));
            assert!(!text.contains("mise install"));
            for kind in HarnessKind::ALL {
                assert!(text.contains(&format!("dev.mj.harnesses.{}=", kind.id())));
            }
            assert_eq!(text.matches("COPY --from").count(), 9);
            assert_eq!(text.matches("LABEL dev.mj.harnesses.").count(), 1);
            for (kind, install) in [
                (HarnessKind::Codex, "npm ci"),
                (HarnessKind::Claude, "npm ci"),
                (HarnessKind::Dsh, "npm install"),
                (HarnessKind::Muse, "npm install"),
                (HarnessKind::Antigravity, "tar -xzOf"),
                (HarnessKind::OpenCode, "tar -xzOf"),
                (HarnessKind::Grok, "zstd -d"),
                (HarnessKind::Kimi, "chmod 0755"),
            ] {
                let dir = format!(
                    "{BAKED_HARNESS_ROOT}/harnesses/{}/{}",
                    kind.id(),
                    pin(kind).install_id
                );
                let layer = text
                    .lines()
                    .find(|line| {
                        line.starts_with("RUN ") && line.contains(&format!("chmod -R a+rX {dir}"))
                    })
                    .unwrap();
                assert!(layer.contains(install), "{layer}");
                assert!(layer.contains(MANIFEST_FILE), "{layer}");
                if harness_uses_node_runtime(kind) {
                    assert!(layer.contains("mkdir -p \"$HOME\""), "{layer}");
                }
            }
            let node_layer = text
                .lines()
                .find(|line| line.starts_with("RUN ") && line.contains(BAKED_NODE_MANIFEST_FILE))
                .unwrap();
            assert!(node_layer.contains("tar -xzf"));
            assert!(node_layer.contains("chmod -R a+rX"));
            assert!(!text.contains(&format!(
                "RUN mkdir -p {BAKED_HARNESS_ROOT}/harnesses/opencode/{}/bin",
                pin(HarnessKind::OpenCode).install_id
            )));
        }
    }

    #[test]
    fn fallback_wrappers_run_baked_tools_with_first_directory_winning() {
        let root = tempfile::tempdir().unwrap();
        // A space and a single quote exercise the quoting at both levels.
        let first = root.path().join("first dir");
        let second = root.path().join("it's second");
        let missing = root.path().join("missing");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        executable(
            &first.join("gh"),
            "#!/bin/sh\necho first-gh \"$0\" \"$@\"\n",
        );
        executable(&second.join("gh"), "#!/bin/sh\necho second-gh\n");
        executable(&second.join("node"), "#!/bin/sh\necho node \"$1\"\n");
        std::fs::write(second.join("data"), "not executable").unwrap();
        let listing = format!(
            "{}\n{}\n\n{}",
            first.display(),
            missing.display(),
            second.display()
        );

        // Building from an earlier session image leaves its wrappers behind;
        // they must neither win over the new recipe's tools nor survive it.
        let bin = root.path().join("out bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("gh"), "#!/bin/sh\necho stale-gh\n");
        executable(&bin.join("removed"), "#!/bin/sh\necho stale-removed\n");

        let output = run_snippet(root.path(), &listing, 0);
        assert!(output.status.success(), "{output:?}");
        assert!(!bin.join("data").exists());
        assert!(!bin.join("removed").exists());
        for name in ["gh", "node"] {
            let mode = std::fs::metadata(bin.join(name))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o755);
        }
        let run = |name: &str, args: &[&str]| {
            let out = Command::new(bin.join(name))
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "{out:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        assert_eq!(
            run("gh", &["a b", "c"]),
            format!("first-gh {} a b c\n", first.join("gh").display())
        );
        assert_eq!(run("node", &["x"]), "node x\n");
    }

    #[test]
    fn fallback_wrappers_accept_an_empty_tool_list() {
        let root = tempfile::tempdir().unwrap();
        let output = run_snippet(root.path(), "", 0);
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            std::fs::read_dir(root.path().join("out bin"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn fallback_wrappers_fail_when_mise_bin_paths_fails() {
        let root = tempfile::tempdir().unwrap();
        let output = run_snippet(root.path(), "", 3);
        assert!(!output.status.success(), "{output:?}");
    }

    #[test]
    fn containerfile_puts_fallback_wrappers_after_shims_and_before_chown() {
        let base = ImageBase {
            reference: format!("docker.io/library/ubuntu:noble@sha256:{}", "a".repeat(64)),
            user: "hel".into(),
            workdir: "/workspace".into(),
            ..Default::default()
        };
        let mise = MiseArtifact {
            url: "https://example.invalid/mise".into(),
            version: "1".into(),
            sha256: "b".repeat(64),
        };
        let text = containerfile(&base, "linux/amd64", &mise, &BTreeMap::new(), &[]).unwrap();
        assert!(text.contains("PATH=/opt/mise/shims:/opt/mj-image/bin:$PATH\n"));
        let install = text
            .find("&& mise install && rm -rf '/opt/mj-image/bin' && mkdir -p '/opt/mj-image/bin'")
            .unwrap();
        assert!(install < text.find("&& chown -R hel").unwrap());
    }
}

pub const BAKED_NODE_LABEL: &str = "dev.mj.harnesses.node";
pub const BAKED_HARNESS_LABEL_PREFIX: &str = "dev.mj.harnesses.";

impl ImageBase {
    pub fn parse_baked_labels(
        labels: &serde_json::Value,
    ) -> (Option<String>, BTreeMap<String, String>) {
        let mut harnesses = BTreeMap::new();
        if let Some(labels) = labels.as_object() {
            for (key, value) in labels {
                if let Some(kind) = key.strip_prefix(BAKED_HARNESS_LABEL_PREFIX)
                    && kind != "node"
                    && let Some(value) = value.as_str()
                {
                    harnesses.insert(kind.to_owned(), value.to_owned());
                }
            }
        }
        (
            labels[BAKED_NODE_LABEL].as_str().map(str::to_owned),
            harnesses,
        )
    }
}

fn emit_embedded_file(text: &mut String, dest: &str, bytes: &[u8], mode: &str) {
    let parent = std::path::Path::new(dest)
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let chunks: Vec<_> = bytes.chunks(48 * 1024).collect();
    for (index, chunk) in chunks
        .iter()
        .copied()
        .chain(chunks.is_empty().then_some(&[][..]))
        .enumerate()
    {
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        let redirect = if index == 0 { ">" } else { ">>" };
        text.push_str(&format!("RUN mkdir -p '{}' && printf '%s' '{encoded}' | base64 -d {redirect} '{dest}' && chmod {mode} '{dest}'\n", parent.display()));
    }
}

/// Keep receipts and permissions in the install layer to avoid copying the
/// installed tree into a second overlayfs layer.
fn install_command_with_receipt(command: &str, dir: &str, receipt: &str, bytes: &[u8]) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    format!(
        "{command} && printf '%s' '{encoded}' | base64 -d > '{dir}/{receipt}' && chmod -R a+rX {dir}"
    )
}

fn emit_install_run(text: &mut String, command: &str, dir: &str, receipt: &str, bytes: &[u8]) {
    text.push_str(&format!(
        "RUN {}\n",
        install_command_with_receipt(command, dir, receipt, bytes)
    ));
}

// Match the runtime installer's pending_install_scripts interpretation, including
// npm versions that return the groups directly rather than under allowScripts.
const INSTALL_SCRIPTS_CHECK: &str = r#"const fs = require('fs'); const s = JSON.parse(fs.readFileSync(process.argv[1], 'utf8')); const groups = Array.isArray(s) ? s : (s.allowScripts || []); const pending = []; for (const group of groups) { if (!Array.isArray(group.changes)) continue; for (const change of group.changes) { if (change.change === 'pending' && change.key) pending.push(change.key); } } if (pending.length) { console.error(pending.join(', ')); process.exit(1); }"#;

/// Emit downloads only in the disposable stage, and copy only changed subtrees.
fn harness_stage(
    base: &ImageBase,
    platform: &str,
    harnesses: &[HarnessKind],
) -> Result<(String, String, String)> {
    let mut kinds = harnesses.to_vec();
    kinds.sort_by_key(|kind| kind.id());
    kinds.dedup();
    if kinds.is_empty() {
        return Ok((String::new(), String::new(), String::new()));
    }
    let key = format!("linux-{}", platform_arch(platform)?);
    let min_node = if kinds.contains(&HarnessKind::Dsh) {
        DSH_ACP_MINIMUM_NODE_VERSION
    } else {
        NODE_MINIMUM_VERSION
    };
    let needs_node = kinds.iter().copied().any(harness_uses_node_runtime);
    let missing_node = needs_node && base.baked_node.as_deref() != Some(NODE_VERSION);
    let missing: Vec<_> = kinds
        .iter()
        .copied()
        .filter(|kind| {
            base.baked_harnesses.get(kind.id()).map(String::as_str) != Some(pin(*kind).install_id)
        })
        .collect();
    let node = crate::runtime_manifests::artifact("node", &key)?;
    let node_dir = format!(
        "{BAKED_HARNESS_ROOT}/node/node-v{NODE_VERSION}-{}",
        node.target
    );
    let mut pins = Vec::new();
    if needs_node {
        pins.push(format!("{BAKED_NODE_LABEL}=\"{NODE_VERSION}\""));
    }
    for kind in kinds {
        pins.push(format!(
            "{BAKED_HARNESS_LABEL_PREFIX}{}=\"{}\"",
            kind.id(),
            pin(kind).install_id
        ));
    }
    let labels = format!("LABEL {}\n", pins.join(" \\\n    "));
    if !missing_node && missing.is_empty() {
        return Ok((String::new(), String::new(), labels));
    }
    let mut stage = format!("FROM {} AS mj-harnesses\nUSER root\n", base.reference);
    let mut copies = String::new();
    if missing_node {
        stage.push_str(&format!(
            "ADD --checksum=sha256:{} {} /tmp/mj-node.tar.gz\nENV PATH={node_dir}/bin:$PATH\n",
            node.sha256, node.url
        ));
        let install = install_command_with_receipt(
            &format!(
                "mkdir -p {BAKED_HARNESS_ROOT}/node && tar -xzf /tmp/mj-node.tar.gz -C {BAKED_HARNESS_ROOT}/node && rm /tmp/mj-node.tar.gz && {node_dir}/bin/node --version && {node_dir}/bin/npm --version && {node_dir}/bin/npx --version"
            ),
            &node_dir,
            BAKED_NODE_MANIFEST_FILE,
            &serde_json::to_vec(&BakedNodeManifest::current())?,
        );
        let command = if let Some(interpreter) = &node.interpreter {
            let node_check = node_version_check_script(min_node);
            format!(
                "if [ -f '{interpreter}' ]; then {install}; else rm -f /tmp/mj-node.tar.gz && mkdir -p {node_dir} && (command -v node >/dev/null 2>&1 && {node_check} && command -v npm >/dev/null 2>&1 && command -v npx >/dev/null 2>&1 || {{ echo 'Mjolnir pinned Node.js requires glibc loader {interpreter}; install Node.js {min_node}+, npm, and npx in this base image' >&2; exit 1; }}); fi"
            )
        } else {
            install
        };
        stage.push_str(&format!("RUN {command}\n"));
        copies.push_str(&format!("COPY --from=mj-harnesses {node_dir} {node_dir}\n"));
    } else if needs_node {
        stage.push_str(&format!("ENV PATH={node_dir}/bin:$PATH\n"));
    }
    for kind in missing {
        let selected = pin(kind);
        let dir = format!(
            "{BAKED_HARNESS_ROOT}/harnesses/{}/{}",
            kind.id(),
            selected.install_id
        );
        let command = match kind {
            HarnessKind::Codex | HarnessKind::Claude => {
                let (package, lock): (&[u8], &[u8]) = if kind == HarnessKind::Codex {
                    (
                        include_bytes!("../assets/harnesses/codex/package.json"),
                        include_bytes!("../assets/harnesses/codex/package-lock.json"),
                    )
                } else {
                    (
                        include_bytes!("../assets/harnesses/claude/package.json"),
                        include_bytes!("../assets/harnesses/claude/package-lock.json"),
                    )
                };
                emit_embedded_file(&mut stage, &format!("{dir}/package.json"), package, "0644");
                emit_embedded_file(
                    &mut stage,
                    &format!("{dir}/package-lock.json"),
                    lock,
                    "0644",
                );
                format!(
                    "cd {dir} && export HOME=/tmp/mj-harness-home && mkdir -p \"$HOME\" && npm ci --omit=dev --no-audit --no-fund --cache /tmp/mj-npm-cache && (if npm install-scripts ls --json > /tmp/mj-scripts.json 2>/tmp/mj-scripts.err; then node -e \"{INSTALL_SCRIPTS_CHECK}\" /tmp/mj-scripts.json; else grep -q 'Unknown command' /tmp/mj-scripts.json /tmp/mj-scripts.err || {{ cat /tmp/mj-scripts.err >&2; exit 1; }}; fi) && ./{} --version && test -x {} && test -x {} && rm -rf /tmp/mj-npm-cache /tmp/mj-scripts.json /tmp/mj-scripts.err \"$HOME\"",
                    selected.cli_entrypoint, selected.entrypoint, selected.cli_entrypoint
                )
            }
            HarnessKind::Dsh | HarnessKind::Muse => {
                let prepare = if kind == HarnessKind::Muse {
                    let artifact = crate::runtime_manifests::artifact("muse", &key)?;
                    stage.push_str(&format!(
                        "ADD --checksum=sha256:{} {} {dir}/bin/muse\n",
                        artifact.sha256, artifact.url
                    ));
                    format!("chmod 0755 {dir}/bin/muse")
                } else {
                    format!("mkdir -p {dir}")
                };
                let (package, version) = if kind == HarnessKind::Dsh {
                    (DSH_ACP_PACKAGE, DSH_ACP_VERSION)
                } else {
                    let bridge = npm_bridge(kind).expect("Muse adapter");
                    (bridge.package, bridge.version)
                };
                let symlink = if kind == HarnessKind::Dsh {
                    format!("ln -s node_modules/.bin/dsh-acp {dir}/dsh-acp && ")
                } else {
                    String::new()
                };
                format!(
                    "{prepare} && export HOME=/tmp/mj-harness-home && mkdir -p \"$HOME\" && npm install --no-save --omit=dev --no-audit --no-fund --cache /tmp/mj-npm-cache --prefix {dir} {package}@{version} && {symlink}test -x {dir}/{} && test -x {dir}/node_modules/.bin/{} && rm -rf /tmp/mj-npm-cache \"$HOME\"",
                    selected.entrypoint,
                    if kind == HarnessKind::Dsh {
                        "dsh-acp"
                    } else {
                        "muse-acp"
                    }
                )
            }
            _ => {
                let artifact = crate::runtime_manifests::artifact(kind.id(), &key)?;
                match kind {
                    HarnessKind::Kimi => {
                        stage.push_str(&format!(
                            "ADD --checksum=sha256:{} {} {dir}/bin/kimi\n",
                            artifact.sha256, artifact.url
                        ));
                        format!("chmod 0755 {dir}/bin/kimi")
                    }
                    HarnessKind::Grok => {
                        stage.push_str(&format!(
                            "ADD --checksum=sha256:{} {} /tmp/mj-grok.zst\n",
                            artifact.sha256, artifact.url
                        ));
                        format!(
                            "mkdir -p {dir}/bin && if command -v zstd >/dev/null 2>&1; then zstd -d -f /tmp/mj-grok.zst -o {dir}/bin/grok; elif [ -x {node_dir}/bin/node ]; then {node_dir}/bin/node -e \"require('stream/promises').pipeline(require('fs').createReadStream('/tmp/mj-grok.zst'),require('zlib').createZstdDecompress(),require('fs').createWriteStream('{dir}/bin/grok')).catch(e=>{{console.error(e);process.exit(1)}})\"; elif command -v apt-get >/dev/null 2>&1; then apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends zstd && zstd -d -f /tmp/mj-grok.zst -o {dir}/bin/grok; elif command -v dnf >/dev/null 2>&1; then dnf install -y zstd && zstd -d -f /tmp/mj-grok.zst -o {dir}/bin/grok; elif command -v apk >/dev/null 2>&1; then apk add --no-cache zstd && zstd -d -f /tmp/mj-grok.zst -o {dir}/bin/grok; elif command -v zypper >/dev/null 2>&1; then zypper --non-interactive install zstd && zstd -d -f /tmp/mj-grok.zst -o {dir}/bin/grok; else echo 'zstd decompressor unavailable' >&2; exit 1; fi && chmod 0755 {dir}/bin/grok && ln -s grok {dir}/bin/agent && rm /tmp/mj-grok.zst"
                        )
                    }
                    HarnessKind::Antigravity | HarnessKind::OpenCode => {
                        let (member, output_dir) = if kind == HarnessKind::Antigravity {
                            ("antigravity", format!("{dir}/bin"))
                        } else {
                            ("package/bin/opencode", dir.clone())
                        };
                        stage.push_str(&format!(
                            "ADD --checksum=sha256:{} {} /tmp/mj-native.tar.gz\n",
                            artifact.sha256, artifact.url
                        ));
                        format!(
                            "mkdir -p {output_dir} && tar -xzOf /tmp/mj-native.tar.gz {member} > {dir}/{} && chmod 0755 {dir}/{} && rm /tmp/mj-native.tar.gz",
                            selected.entrypoint, selected.entrypoint
                        )
                    }
                    _ => unreachable!(),
                }
            }
        };
        emit_install_run(
            &mut stage,
            &command,
            &dir,
            MANIFEST_FILE,
            &serde_json::to_vec(&ManagedHarnessManifest::for_harness(kind))?,
        );
        copies.push_str(&format!("COPY --from=mj-harnesses {dir} {dir}\n"));
    }
    Ok((stage, copies, labels))
}

pub fn session_base_containerfile(base: &ImageBase, platform: &str) -> Result<String> {
    ensure!(
        base.reference.contains("@sha256:") && !base.reference.contains(['\r', '\n']),
        "base image must be pinned by digest"
    );
    ensure!(!base.user.contains(['\r', '\n']), "invalid user");
    ensure!(!platform.contains(['\r', '\n']), "invalid platform");
    ensure!(!base.workdir.contains(['\r', '\n']), "invalid workdir");
    let user = base.user.split(':').next().unwrap_or_default();
    if !base.user.is_empty() && user != "root" && user != "0" {
        ensure!(
            base.user
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte)),
            "unsupported base image user {:?}",
            base.user
        );
    }

    let (stage, copies, labels) = harness_stage(base, platform, &HarnessKind::ALL)?;
    Ok(format!(
        "{stage}FROM {}\nUSER root\n{copies}{labels}USER {}\nWORKDIR {}\n",
        base.reference,
        if base.user.is_empty() {
            "root"
        } else {
            &base.user
        },
        serde_json::to_string(if base.workdir.is_empty() {
            "/"
        } else {
            &base.workdir
        })?
    ))
}
