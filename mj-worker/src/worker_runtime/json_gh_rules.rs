//! Session-owned native GitHub CLI permissions for the JSON-configured harnesses.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstObjectProp, CstRootNode},
};
use mj_core::config::{ExecutionPolicy, HarnessKind};

const OPENCODE_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const GH_COMMANDS: [&str; 2] = ["gh", "gh *"];
const MIMO_LAYERS: [&str; 3] = ["config.json", "mimocode.json", "mimocode.jsonc"];
const MIMO_PENDING: &str = ".mj-gh-permissions.pending.json";

pub(super) fn configure_json_gh_rules(
    root: &Path,
    session_id: &str,
    home: &Path,
    harness: HarnessKind,
    policy: ExecutionPolicy,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    if policy != ExecutionPolicy::ConfiguredApprovals {
        return Ok(());
    }
    match harness {
        HarnessKind::OpenCode => configure_opencode(environment),
        HarnessKind::Mimo if super::subagents::private_staged_home(root, session_id, home) => {
            configure_mimo(home)
        }
        _ => Ok(()),
    }
}

fn document(text: &str) -> Result<(CstRootNode, CstObject)> {
    let document = CstRootNode::parse(text, &ParseOptions::default())
        .context("parse native gh permission configuration")?;
    let object = document
        .object_value()
        .context("native gh permission configuration must be an object")?;
    Ok((document, object))
}

fn string_value(node: Option<CstNode>) -> Option<String> {
    node?.as_string_lit()?.decoded_value().ok()
}

fn configure_opencode(environment: &mut BTreeMap<String, String>) -> Result<()> {
    let original = environment
        .get(OPENCODE_CONTENT)
        .map(String::as_str)
        .unwrap_or("{}");
    let (document, object) = document(original).context(OPENCODE_CONTENT)?;
    let rules = object
        .array_value_or_create("permissions")
        .context("OPENCODE_CONFIG_CONTENT permissions must be an array")?;
    // OpenCode 2.2.1 concatenates native permission arrays across config documents.
    // Keeping this as its own document preserves even scalar legacy defaults.
    let elements = rules.elements();
    let final_rules = elements.len() >= GH_COMMANDS.len()
        && elements[elements.len() - GH_COMMANDS.len()..]
            .iter()
            .zip(GH_COMMANDS)
            .all(|(node, command)| {
                node.as_object().is_some_and(|rule| {
                    string_value(rule.get("action").and_then(|p| p.value())).as_deref()
                        == Some("bash")
                        && string_value(rule.get("resource").and_then(|p| p.value())).as_deref()
                            == Some(command)
                        && string_value(rule.get("effect").and_then(|p| p.value())).as_deref()
                            == Some("allow")
                })
            });
    // An earlier identical allow may be shadowed by later restrictions.
    if !final_rules {
        for command in GH_COMMANDS {
            rules.append(CstInputValue::Object(vec![
                ("action".into(), "bash".into()),
                ("resource".into(), command.into()),
                ("effect".into(), "allow".into()),
            ]));
        }
    }
    let updated = document.to_string();
    if updated != original {
        environment.insert(OPENCODE_CONTENT.into(), updated);
    }
    Ok(())
}

fn configure_mimo(home: &Path) -> Result<()> {
    let directory = home.join("config");
    match std::fs::symlink_metadata(&directory) {
        Ok(metadata) => ensure!(
            metadata.file_type().is_dir(),
            "MiMo configuration {} must be a real directory",
            directory.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(&directory)
                .with_context(|| format!("create {}", directory.display()))?;
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", directory.display()));
        }
    }
    finish_mimo_permissions(&directory)?;
    // MiMo 0.1.16 normalizes top-level scalar actions before merging layers.
    // Object merges retain key positions; a scalar replaces an entire object.
    let mut layers = Vec::new();
    let mut effective = Vec::new();
    for name in MIMO_LAYERS {
        let path = directory.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                ensure!(
                    metadata.file_type().is_file(),
                    "MiMo configuration {} must be a regular file",
                    path.display()
                );
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("read {}", path.display()))?;
                let (document, object) =
                    document(&text).with_context(|| path.display().to_string())?;
                if let Some(permission) = object.get("permission") {
                    merge_permission(
                        &mut effective,
                        read_permission(permission.value().context("missing permission value")?)?,
                    );
                }
                layers.push((path, text, document, object));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("inspect {}", path.display())),
        }
    }
    if layers.is_empty() {
        let (document, object) = document("{}\n")?;
        layers.push((
            directory.join("mimocode.jsonc"),
            "{}\n".into(),
            document,
            object,
        ));
    }

    // Reproduce every rule applicable to Bash, including later tool wildcards.
    // Moving an old Bash map alone would reactivate rules those wildcards hid.
    let mut bash = Vec::new();
    for (tool, rule) in &effective {
        if native_wildcard_match("bash", tool) {
            match rule {
                PermissionRule::Action(action) => append_pattern(&mut bash, "*", action),
                PermissionRule::Patterns(patterns) => {
                    for (pattern, action) in patterns {
                        append_pattern(&mut bash, pattern, action);
                    }
                }
            }
        }
    }
    for command in GH_COMMANDS {
        append_pattern(&mut bash, command, "allow");
    }
    effective.retain(|(tool, _)| tool != "bash");
    effective.push(("bash".into(), PermissionRule::Patterns(bash)));
    let already_normalized = highest_permission_matches(&layers.last().unwrap().3, &effective)?;
    let normalized = CstInputValue::Object(
        effective
            .into_iter()
            .map(|(tool, rule)| (tool, rule.into_input()))
            .collect(),
    );
    let highest = layers.last().unwrap();
    if let Some(permission) = highest.3.get("permission") {
        // Avoid formatting churn on an already normalized profile.
        if !already_normalized {
            permission.set_value(normalized);
        }
    } else {
        highest.3.append("permission", normalized);
    }
    for (_, _, _, object) in layers.iter().take(layers.len() - 1) {
        if let Some(permission) = object.get("permission") {
            permission.remove();
        }
    }
    let mut files = Vec::new();
    for (path, original, document, _) in layers.iter().rev() {
        let updated = retain_comments(original, &document.to_string())?;
        if updated != *original {
            files.push(MimoPermissionFile {
                name: path.file_name().unwrap().to_str().unwrap().to_owned(),
                body: updated,
            });
        }
    }
    if !files.is_empty() {
        let pending = directory.join(MIMO_PENDING);
        ensure!(
            super::subagents::regular_file_or_absent(&pending),
            "MiMo permission recovery file {} must be a regular file",
            pending.display()
        );
        // Persist the entire decision before any configuration changes. A
        // retry completes this batch before interpreting partially edited layers.
        let batch = MimoPermissionBatch { version: 1, files };
        mj_core::config::atomic_write(&pending, &serde_json::to_vec(&batch)?)
            .with_context(|| format!("record MiMo permission changes {}", pending.display()))?;
        finish_mimo_permissions(&directory)?;
    }
    Ok(())
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MimoPermissionBatch {
    version: u8,
    files: Vec<MimoPermissionFile>,
}

#[derive(serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct MimoPermissionFile {
    name: String,
    body: String,
}

fn finish_mimo_permissions(directory: &Path) -> Result<()> {
    let pending = directory.join(MIMO_PENDING);
    ensure!(
        super::subagents::regular_file_or_absent(&pending),
        "MiMo permission recovery file {} must be a regular file",
        pending.display()
    );
    let batch: MimoPermissionBatch = match std::fs::read(&pending) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("read MiMo permission recovery {}", pending.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("read {}", pending.display())),
    };
    ensure!(
        batch.version == 1,
        "unsupported MiMo permission recovery version"
    );
    let mut names = std::collections::BTreeSet::new();
    for file in &batch.files {
        ensure!(
            MIMO_LAYERS.contains(&file.name.as_str()) && names.insert(&file.name),
            "invalid MiMo permission recovery filename"
        );
        let path = directory.join(&file.name);
        ensure!(
            super::subagents::regular_file_or_absent(&path),
            "MiMo configuration {} must be a regular file",
            path.display()
        );
    }
    for file in batch.files {
        let path = directory.join(file.name);
        mj_core::config::atomic_write(&path, file.body.as_bytes())
            .with_context(|| format!("complete MiMo permission changes {}", path.display()))?;
    }
    std::fs::remove_file(&pending)
        .with_context(|| format!("complete MiMo permission recovery {}", pending.display()))
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PermissionRule {
    Action(String),
    Patterns(Vec<(String, String)>),
}

type Permissions = Vec<(String, PermissionRule)>;

impl PermissionRule {
    fn into_input(self) -> CstInputValue {
        match self {
            Self::Action(action) => action.into(),
            Self::Patterns(patterns) => CstInputValue::Object(
                patterns
                    .into_iter()
                    .map(|(pattern, action)| (pattern, action.into()))
                    .collect(),
            ),
        }
    }
}

fn action(node: Option<CstNode>) -> Result<String> {
    let value = string_value(node).context("permission action must be a string")?;
    ensure!(
        matches!(value.as_str(), "ask" | "allow" | "deny"),
        "permission action must be ask, allow, or deny"
    );
    Ok(value)
}

fn read_permission(node: CstNode) -> Result<Permissions> {
    let Some(object) = node.as_object() else {
        return Ok(vec![(
            "*".into(),
            PermissionRule::Action(action(Some(node))?),
        )]);
    };
    native_properties(&object)
        .into_iter()
        .map(|property| {
            let tool = property
                .decoded_name()
                .context("invalid permission tool name")?;
            let rule = if let Some(patterns) = property.object_value() {
                PermissionRule::Patterns(
                    native_properties(&patterns)
                        .into_iter()
                        .map(|pattern| {
                            Ok((
                                pattern
                                    .decoded_name()
                                    .context("invalid permission pattern")?,
                                action(pattern.value())?,
                            ))
                        })
                        .collect::<Result<_>>()?,
                )
            } else {
                PermissionRule::Action(action(property.value())?)
            };
            Ok((tool, rule))
        })
        .collect()
}

fn native_properties(object: &CstObject) -> Vec<CstObjectProp> {
    // JSON.parse keeps a duplicate key's first position and last value;
    // Object.entries enumerates array-index keys before other string keys.
    let mut result: Vec<CstObjectProp> = Vec::new();
    for property in object.properties() {
        if let Some(existing) = result
            .iter_mut()
            .find(|existing| existing.decoded_name() == property.decoded_name())
        {
            *existing = property;
        } else {
            result.push(property);
        }
    }
    result.sort_by_key(|property| {
        let index = property.decoded_name().and_then(|name| {
            name.parse::<u32>()
                .ok()
                .filter(|index| *index != u32::MAX && index.to_string() == name)
        });
        (index.is_none(), index)
    });
    result
}

fn highest_permission_matches(object: &CstObject, expected: &Permissions) -> Result<bool> {
    object
        .get("permission")
        .map(|property| {
            read_permission(property.value().context("missing permission value")?)
                .map(|actual| actual == *expected)
        })
        .unwrap_or(Ok(false))
}

fn merge_permission(target: &mut Permissions, source: Permissions) {
    for (tool, incoming) in source {
        if let Some((_, existing)) = target.iter_mut().find(|(name, _)| *name == tool) {
            match (existing, incoming) {
                (PermissionRule::Patterns(existing), PermissionRule::Patterns(incoming)) => {
                    for (pattern, action) in incoming {
                        if let Some((_, value)) =
                            existing.iter_mut().find(|(name, _)| *name == pattern)
                        {
                            *value = action;
                        } else {
                            existing.push((pattern, action));
                        }
                    }
                }
                (existing, incoming) => *existing = incoming,
            }
        } else {
            target.push((tool, incoming));
        }
    }
}

fn append_pattern(patterns: &mut Vec<(String, String)>, pattern: &str, action: &str) {
    patterns.retain(|(existing, _)| existing != pattern);
    patterns.push((pattern.into(), action.into()));
}

// Pinned MiMo Wildcard.match: slash normalization, * and ?, optional trailing
// " *", case-insensitive on Windows. No character classes or shell expansion.
fn native_wildcard_match(value: &str, pattern: &str) -> bool {
    let value = value.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    let (value, pattern) = if cfg!(windows) {
        (value.to_lowercase(), pattern.to_lowercase())
    } else {
        (value, pattern)
    };
    let matches = |pattern: &str| {
        let value: Vec<_> = value.chars().collect();
        let mut matched = vec![false; value.len() + 1];
        matched[0] = true;
        for token in pattern.chars() {
            if token == '*' {
                for index in 1..matched.len() {
                    matched[index] |= matched[index - 1];
                }
            } else {
                for index in (1..matched.len()).rev() {
                    matched[index] =
                        matched[index - 1] && (token == '?' || token == value[index - 1]);
                }
                matched[0] = false;
            }
        }
        matched[value.len()]
    };
    matches(&pattern) || pattern.strip_suffix(" *").is_some_and(matches)
}

fn retain_comments(original: &str, updated: &str) -> Result<String> {
    fn comments(node: CstNode, output: &mut Vec<String>) {
        if node.as_comment().is_some() {
            output.push(node.to_string());
        } else {
            for child in node.children() {
                comments(child, output);
            }
        }
    }
    let mut before = Vec::new();
    let mut after = Vec::new();
    comments(
        CstRootNode::parse(original, &ParseOptions::default())?.into(),
        &mut before,
    );
    comments(
        CstRootNode::parse(updated, &ParseOptions::default())?.into(),
        &mut after,
    );
    let mut retained = String::new();
    for comment in before {
        if let Some(index) = after.iter().position(|existing| *existing == comment) {
            after.remove(index);
        } else {
            retained.push_str(&comment);
            retained.push('\n');
        }
    }
    retained.push_str(updated);
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(
        home: &Path,
        harness: HarnessKind,
        environment: &mut BTreeMap<String, String>,
    ) -> Result<()> {
        configure_json_gh_rules(
            home.parent().unwrap(),
            "test-session",
            home,
            harness,
            ExecutionPolicy::ConfiguredApprovals,
            environment,
        )
    }

    #[test]
    fn opencode_native_overlay_preserves_jsonc_and_legacy_defaults_on_legacy_home() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("legacy-home");
        let original = r#"{
  // retain the user's scalar default and native rule order
  "permission": "ask",
  "permissions": [
    {"action":"bash","resource":"gh *","effect":"allow"},
    {"action":"bash","resource":"*","effect":"deny"},
    {"action":"bash","resource":"rm *","effect":"deny"},
    {"action":"read","resource":".env","effect":"ask"},
  ],
  "providers": {"z":{}, "a":{}},
}"#;
        let mut environment = BTreeMap::from([(OPENCODE_CONTENT.into(), original.into())]);
        configure(&home, HarnessKind::OpenCode, &mut environment).unwrap();
        let updated = environment[OPENCODE_CONTENT].clone();
        assert!(updated.contains("// retain the user's scalar default and native rule order"));
        assert!(updated.contains("\"permission\": \"ask\""));
        assert!(updated.contains("\"providers\": {\"z\":{}, \"a\":{}}"));
        let (_, object) = document(&updated).unwrap();
        let rules = object.array_value("permissions").unwrap().elements();
        let resources: Vec<_> = rules
            .iter()
            .map(|node| {
                string_value(node.as_object().unwrap().get("resource").unwrap().value()).unwrap()
            })
            .collect();
        assert_eq!(resources, ["gh *", "*", "rm *", ".env", "gh", "gh *"]);
        configure(&home, HarnessKind::OpenCode, &mut environment).unwrap();
        assert_eq!(environment[OPENCODE_CONTENT], updated);
        assert!(!home.exists());
    }

    #[test]
    fn mimo_jsonc_rules_follow_wildcards_and_preserve_comments_and_order() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir_all(home.join("config")).unwrap();
        let path = home.join("config/mimocode.jsonc");
        std::fs::write(
            &path,
            r#"{
  "provider": {"z":{}, "a":{}},
  "permission": {
    "read": "deny",
    "bash": {
      "gh *": "ask", // gh comment
      "git *": "allow",
      // keep the default
      "*": "deny",
      "gh": "ask"
    }
  }
}"#,
        )
        .unwrap();
        std::fs::write(
            home.join("config/mimocode.json"),
            r#"{"permission":{"edit":"ask"}}"#,
        )
        .unwrap();
        let mut environment = BTreeMap::new();
        configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
        let updated = std::fs::read_to_string(&path).unwrap();
        assert!(updated.contains("// gh comment"));
        assert!(updated.contains("// keep the default"));
        assert!(updated.contains("\"provider\": {\"z\":{}, \"a\":{}}"));
        let (_, object) = document(&updated).unwrap();
        let permission = object.object_value("permission").unwrap();
        assert_eq!(
            string_value(permission.get("read").unwrap().value()).as_deref(),
            Some("deny")
        );
        let bash = permission.object_value("bash").unwrap();
        assert_eq!(
            bash.properties()
                .iter()
                .map(|p| p.decoded_name().unwrap())
                .collect::<Vec<_>>(),
            ["git *", "*", "gh", "gh *"]
        );
        assert_eq!(
            string_value(bash.get("*").unwrap().value()).as_deref(),
            Some("deny")
        );
        assert_eq!(
            string_value(bash.get("gh *").unwrap().value()).as_deref(),
            Some("allow")
        );
        configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        let lower = std::fs::read_to_string(home.join("config/mimocode.json")).unwrap();
        assert!(document(&lower).unwrap().1.get("permission").is_none());
        assert_eq!(
            string_value(permission.get("edit").unwrap().value()).as_deref(),
            Some("ask")
        );
    }

    #[test]
    fn mimo_scalar_defaults_survive_native_rule_insertion_and_layering() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        std::fs::create_dir_all(home.join("config")).unwrap();
        let path = home.join("config/mimocode.jsonc");
        let mut environment = BTreeMap::new();
        // Top-level defaults and Bash defaults are both supported native forms.
        for original in [
            r#"{"permission":"deny"}"#,
            r#"{"permission":{"bash":"ask","edit":"deny"}}"#,
        ] {
            std::fs::write(&path, original).unwrap();
            configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
            let (_, object) = document(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let permission = object.object_value("permission").unwrap();
            let bash = permission.object_value("bash").unwrap();
            if original.contains("bash") {
                assert_eq!(
                    string_value(bash.get("*").unwrap().value()).as_deref(),
                    Some("ask")
                );
                assert_eq!(
                    string_value(permission.get("edit").unwrap().value()).as_deref(),
                    Some("deny")
                );
            } else {
                assert_eq!(
                    string_value(permission.get("*").unwrap().value()).as_deref(),
                    Some("deny")
                );
            }
        }
        std::fs::write(
            home.join("config/mimocode.json"),
            r#"{"permission":{"bash":"deny"}}"#,
        )
        .unwrap();
        std::fs::write(&path, r#"{"permission":{"edit":"ask"}}"#).unwrap();
        configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
        let (_, object) = document(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let bash = object
            .object_value("permission")
            .unwrap()
            .object_value("bash")
            .unwrap();
        assert_eq!(
            string_value(bash.get("*").unwrap().value()).as_deref(),
            Some("deny")
        );
    }

    #[test]
    fn mimo_interrupted_consolidation_recovers_before_interpreting_mixed_layers() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        let directory = home.join("config");
        std::fs::create_dir_all(&directory).unwrap();
        let lower = directory.join("config.json");
        let highest = directory.join("mimocode.jsonc");
        let original_lower = r#"{"permission":{"bash":{"*":"ask","rm *":"allow"}}}"#;
        std::fs::write(&lower, original_lower).unwrap();
        std::fs::write(&highest, r#"{"permission":{"bash":"deny"}}"#).unwrap();
        let mut environment = BTreeMap::new();
        configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
        let files: Vec<_> = ["mimocode.jsonc", "config.json"]
            .into_iter()
            .map(|name| MimoPermissionFile {
                name: name.into(),
                body: std::fs::read_to_string(directory.join(name)).unwrap(),
            })
            .collect();
        let expected_highest = std::fs::read(&highest).unwrap();
        // Model a crash after the upper layer changed but before the lower
        // policy was removed, followed by an I/O failure on the first retry.
        std::fs::remove_file(&lower).unwrap();
        std::fs::create_dir(&lower).unwrap();
        let pending = directory.join(MIMO_PENDING);
        std::fs::write(
            &pending,
            serde_json::to_vec(&MimoPermissionBatch { version: 1, files }).unwrap(),
        )
        .unwrap();
        assert!(configure(&home, HarnessKind::Mimo, &mut environment).is_err());
        assert!(pending.is_file());
        std::fs::remove_dir(&lower).unwrap();
        std::fs::write(&lower, original_lower).unwrap();
        configure(&home, HarnessKind::Mimo, &mut environment).unwrap();
        assert!(!pending.exists());
        assert_eq!(std::fs::read(&highest).unwrap(), expected_highest);
        let effective = effective_profile(&home);
        assert_eq!(native_decision(&effective, "bash", "rm file"), "deny");
        assert_eq!(
            native_decision(&effective, "bash", "gh issue list"),
            "allow"
        );

        let outside = root.path().join("outside.json");
        std::fs::write(&outside, "unchanged").unwrap();
        std::fs::write(
            &pending,
            serde_json::to_vec(&MimoPermissionBatch {
                version: 1,
                files: vec![MimoPermissionFile {
                    name: "../../outside.json".into(),
                    body: "changed".into(),
                }],
            })
            .unwrap(),
        )
        .unwrap();
        assert!(configure(&home, HarnessKind::Mimo, &mut environment).is_err());
        assert_eq!(std::fs::read_to_string(outside).unwrap(), "unchanged");
    }

    fn effective_profile(home: &Path) -> Permissions {
        let mut effective = Vec::new();
        for name in ["config.json", "mimocode.json", "mimocode.jsonc"] {
            if let Ok(text) = std::fs::read_to_string(home.join("config").join(name)) {
                let (_, object) = document(&text).unwrap();
                if let Some(permission) = object.get("permission") {
                    merge_permission(
                        &mut effective,
                        read_permission(permission.value().unwrap()).unwrap(),
                    );
                }
            }
        }
        effective
    }

    fn native_decision<'a>(permissions: &'a Permissions, tool: &str, resource: &str) -> &'a str {
        // The pinned evaluator flattens tools and patterns in insertion order
        // and selects the last matching rule, defaulting to ask.
        let mut decision = "ask";
        for (pattern, rule) in permissions {
            if !native_wildcard_match(tool, pattern) {
                continue;
            }
            match rule {
                PermissionRule::Action(action) => decision = action,
                PermissionRule::Patterns(patterns) => {
                    for (pattern, action) in patterns {
                        if native_wildcard_match(resource, pattern) {
                            decision = action;
                        }
                    }
                }
            }
        }
        decision
    }

    // Hard-won: native-gh: converting a higher Bash scalar resurrected lower allow rules.
    #[test]
    fn mimo_consolidation_preserves_effective_policy_across_ordered_private_layers() {
        let cases = [
            (
                r#"{"permission":{"bash":{"*":"ask","rm *":"allow"},"read":"deny"}}"#,
                r#"{"permission":{"bash":"deny"}}"#,
                r#"{"provider":{"z":{},"a":{}}}"#,
                "deny",
            ),
            (
                r#"{"permission":{"bash":{"gh *":"ask","*":"deny","rm *":"allow"}}}"#,
                r#"{"permission":{"bash":{"*":"ask","git *":"allow"}}}"#,
                r#"{"permission":{"edit":"deny"}}"#,
                "allow",
            ),
            (
                r#"{"permission":{"bash":{"rm *":"allow","git *":"allow"},"*":"deny"}}"#,
                r#"{"permission":{"read":"ask"}}"#,
                r#"{"permission":{"b?sh":{"cargo *":"ask"}}}"#,
                "deny",
            ),
        ];
        for (lower, middle, upper, expected_rm) in cases {
            let root = tempfile::tempdir().unwrap();
            let home = root.path().join("profile");
            std::fs::create_dir_all(home.join("config")).unwrap();
            for (name, text) in [
                ("config.json", lower),
                ("mimocode.json", middle),
                ("mimocode.jsonc", upper),
            ] {
                std::fs::write(home.join("config").join(name), text).unwrap();
            }
            let before = effective_profile(&home);
            assert_eq!(native_decision(&before, "bash", "rm file"), expected_rm);
            configure(&home, HarnessKind::Mimo, &mut BTreeMap::new()).unwrap();
            let after = effective_profile(&home);
            for tool in ["bash", "read", "edit", "task"] {
                for resource in [
                    "rm file",
                    "git status",
                    "cargo check",
                    "echo ok",
                    ".env",
                    "ghish",
                    "gh-other list",
                ] {
                    assert_eq!(
                        native_decision(&after, tool, resource),
                        native_decision(&before, tool, resource),
                        "{tool}({resource}) changed in {lower} / {middle} / {upper}"
                    );
                }
            }
            for command in ["gh", "gh pr list"] {
                assert_eq!(native_decision(&after, "bash", command), "allow");
            }
            let paths = ["config.json", "mimocode.json", "mimocode.jsonc"]
                .map(|name| home.join("config").join(name));
            let first = paths
                .each_ref()
                .map(|path| std::fs::read_to_string(path).unwrap());
            configure(&home, HarnessKind::Mimo, &mut BTreeMap::new()).unwrap();
            assert_eq!(
                paths
                    .each_ref()
                    .map(|path| std::fs::read_to_string(path).unwrap()),
                first
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn mimo_does_not_follow_linked_config_directories_or_files() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("profile");
        let outside = root.path().join("outside");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, home.join("config")).unwrap();
        assert!(configure(&home, HarnessKind::Mimo, &mut BTreeMap::new()).is_err());
        std::fs::remove_file(home.join("config")).unwrap();
        std::fs::create_dir(home.join("config")).unwrap();
        let outside_file = outside.join("mimocode.jsonc");
        std::fs::write(&outside_file, "{}").unwrap();
        symlink(&outside_file, home.join("config/mimocode.jsonc")).unwrap();
        assert!(configure(&home, HarnessKind::Mimo, &mut BTreeMap::new()).is_err());
        assert_eq!(std::fs::read_to_string(outside_file).unwrap(), "{}");
    }
}
