//! Session-owned native GitHub CLI permissions for the JSON-configured harnesses.

use std::collections::BTreeMap;

use anyhow::{Context, Result, ensure};
use jsonc_parser::{
    ParseOptions,
    cst::{CstInputValue, CstNode, CstObject, CstRootNode},
};
use mj_core::config::{ExecutionPolicy, HarnessKind};

const OPENCODE_CONTENT: &str = "OPENCODE_CONFIG_CONTENT";
const GH_COMMANDS: [&str; 2] = ["gh", "gh *"];

pub(super) fn configure_json_gh_rules(
    harness: HarnessKind,
    policy: ExecutionPolicy,
    environment: &mut BTreeMap<String, String>,
) -> Result<()> {
    if policy != ExecutionPolicy::ConfiguredApprovals {
        return Ok(());
    }
    match harness {
        HarnessKind::Muse => {
            let mut prefixes: Vec<Vec<String>> = environment
                .get("MUSE_ACP_ALLOW_COMMAND_PREFIXES")
                .map(|value| serde_json::from_str(value))
                .transpose()
                .context("parse Muse native command prefixes")?
                .unwrap_or_default();
            ensure!(
                prefixes
                    .iter()
                    .all(|prefix| !prefix.is_empty() && prefix.iter().all(|word| !word.is_empty())),
                "Muse native command prefixes must contain nonempty argv words"
            );
            if !prefixes.iter().any(|prefix| prefix.as_slice() == ["gh"]) {
                prefixes.push(vec!["gh".into()]);
            }
            environment.insert(
                "MUSE_ACP_ALLOW_COMMAND_PREFIXES".into(),
                serde_json::to_string(&prefixes)?,
            );
            Ok(())
        }
        HarnessKind::OpenCode => configure_opencode(environment),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn configure(harness: HarnessKind, environment: &mut BTreeMap<String, String>) -> Result<()> {
        configure_json_gh_rules(harness, ExecutionPolicy::ConfiguredApprovals, environment)
    }

    #[test]
    fn opencode_native_overlay_preserves_jsonc_and_legacy_defaults() {
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
        configure(HarnessKind::OpenCode, &mut environment).unwrap();
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
        configure(HarnessKind::OpenCode, &mut environment).unwrap();
        assert_eq!(environment[OPENCODE_CONTENT], updated);
    }
}
