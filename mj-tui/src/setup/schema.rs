//! Editable defaults and labels for every field in the user configuration.
use serde_json::{Value, json};

pub(super) fn defaults(path: &[String], value: &Value) -> Value {
    let key = path.last().map(String::as_str).unwrap_or("");
    match path.first().map(String::as_str).unwrap_or("") {
        "" => {
            json!({"sessions_side":"left", "spinner":"scan", "theme":"midnight", "advanced":{}, "phone":{}, "review":{}, "sessionwiki":{}, "subagents":{}, "build_cache":{}, "profiles":{}, "targets":{}, "bundles":{}})
        }
        "phone" => {
            json!({"enabled":true,"bind":"127.0.0.1:3765","tailscale_detect":true,"tls_cert":null,"tls_key":null})
        }
        "advanced" => {
            json!({"detailed_activity_clocks":false,"show_stopped_sessions":false})
        }
        "review" => {
            json!({"enabled":false,"tier":"quick","profile":null,"model":null,"effort":null})
        }
        "sessionwiki" => {
            json!({"archive_after_days":null})
        }
        "subagents" if path.len() == 1 => {
            json!({"enabled":true,"max_concurrent":6,"eligible_profiles":{}})
        }
        "build_cache" if path.len() == 1 => {
            json!({"enabled": true})
        }
        "profiles" if path.len() == 2 => {
            json!({"enabled":true,"kind":"codex","home":"","environment":{},"context_window_bytes":null,"guardian_review_model":null})
        }
        "targets" if path.len() == 2 => {
            target_defaults(value["kind"].as_str().unwrap_or("local-bare"))
        }
        "targets" if key == "workspace_storage" => {
            if value["kind"] == "host-helper" {
                json!({"kind":"host-helper","root":"","helper":[]})
            } else {
                json!({"kind":"podman-volume"})
            }
        }
        "bundles" if path.len() == 2 => json!({"primary_repo":"main","repositories":[]}),
        "bundles" if path.len() == 4 => repository_default(),
        _ => json!({}),
    }
}

pub(super) fn repository_default() -> Value {
    json!({"id":"main","github":null,"local":null,"destination":"main"})
}

fn target_defaults(kind: &str) -> Value {
    let mut fields = json!({"kind":kind});
    if matches!(
        kind,
        "local-podman" | "local-docker" | "apple-container" | "ssh-podman" | "ssh-docker"
    ) {
        fields.as_object_mut().unwrap().extend(json!({
            "image":mj_client::target::DEFAULT_IMAGE,"pull_policy":"auto","platform":null,
            "cpus":null,"memory":null,"environment":{},"workspace_storage":{"kind":"podman-volume"},
            "build_cache":{"enabled":null,"directory":null,"max_size":null}
        }).as_object().unwrap().clone());
    }
    if kind.starts_with("ssh-") {
        fields.as_object_mut().unwrap().extend(
            json!({"host":"","user":null,"identity_file":null,"extra_args":[]})
                .as_object()
                .unwrap()
                .clone(),
        );
    }
    if kind == "ssh-bare" {
        fields.as_object_mut().unwrap().extend(
            json!({"permissions":"guardian","workspace_prefix":".local/share/hel/workspaces"})
                .as_object()
                .unwrap()
                .clone(),
        );
    }
    if kind == "aws-ec2" {
        fields.as_object_mut().unwrap().extend(json!({"aws_profile":null,"region":"","launch_template":"","launch_template_version":null,"ssh_user":"ubuntu","address_source":"public-dns","identity_file":null,"ssh_args":[]}).as_object().unwrap().clone());
    }
    fields
}

/// Insert omitted optional fields, preserving all stored values.
pub(super) fn expand(value: &mut Value, path: &mut Vec<String>) {
    let missing = defaults(path, value);
    if let Some(object) = value.as_object_mut() {
        for (key, default) in missing.as_object().unwrap() {
            object.entry(key.clone()).or_insert_with(|| default.clone());
        }
        for (key, child) in object {
            path.push(key.clone());
            expand(child, path);
            path.pop();
        }
    } else if let Some(array) = value.as_array_mut() {
        for (index, child) in array.iter_mut().enumerate() {
            path.push(index.to_string());
            expand(child, path);
            path.pop();
        }
    }
}

pub(super) fn label(key: &str) -> String {
    match key {
        "interface" => "Interface",
        "sessions_side" => "Session sidebar position",
        "spinner" => "Activity animation",
        "advanced" => "Advanced",
        "detailed_activity_clocks" => "Detailed activity clocks",
        "show_stopped_sessions" => "Show stopped sessions",
        "theme" => "Theme",
        "phone" => "Web Access",
        "review" => "Code Review",
        "sessionwiki" => "SessionWiki",
        "archive_after_days" => "Archive after (days)",
        "subagents" => "Sub-agents",
        "build_cache" => "Build cache (mbx)",
        "directory" => "Cache directory",
        "max_size" => "Cache size limit",
        "max_concurrent" => "Maximum concurrent children",
        "eligible_profiles" => "Additional eligible profiles",
        "profiles" => "Agent Profiles",
        "targets" => "Machines and Runtimes",
        "bundles" => "Projects",
        "enabled" => "Enabled",
        "profile" => "Agent profile",
        "kind" => "Type",
        "home" => "Account directory",
        "environment" => "Environment variables",
        "context_window_bytes" => "Context budget (bytes)",
        "guardian_review_model" => "Guardian review model",
        "bind" => "Listen address and port",
        "tailscale_detect" => "Detect Tailscale",
        "tls_cert" => "TLS certificate file",
        "tls_key" => "TLS private key file",
        "tier" => "Review depth",
        "model" => "Review model",
        "effort" => "Review effort",
        "image" => "Container image",
        "pull_policy" => "Download image policy",
        "platform" => "CPU platform",
        "cpus" => "CPU limit",
        "memory" => "Memory limit",
        "workspace_storage" => "Workspace storage",
        "host" => "SSH host",
        "user" | "ssh_user" => "SSH username",
        "identity_file" => "SSH key file",
        "extra_args" | "ssh_args" => "SSH arguments",
        "permissions" => "Agent permissions",
        "workspace_prefix" => "Remote workspace directory",
        "aws_profile" => "AWS account profile",
        "region" => "AWS region",
        "launch_template" => "EC2 launch template",
        "launch_template_version" => "Template version",
        "address_source" => "Connect using",
        "primary_repo" => "Main repository",
        "repositories" => "Repositories",
        "id" => "Name",
        "github" => "GitHub owner/repository",
        "local" => "Local repository directory",
        "destination" => "Checkout folder",
        "root" => "Storage directory",
        "helper" => "Storage helper command",
        _ => key,
    }
    .to_owned()
}

/// What an empty (JSON null) value means for this setting: the effect the
/// value actually has, never a placeholder. A value the runtime resolves from
/// the host is filled in by the caller through `value_summary`'s `automatic`
/// argument instead, because only the dialog can measure it.
pub(super) fn null_label(path: &[String], draft: &Value) -> String {
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        // An empty archive window never archives; there is no hidden number.
        ["sessionwiki", "archive_after_days"] => "Never".to_owned(),
        // The backend emits no CPU or memory flag, so the container competes
        // for the whole machine.
        ["targets", _, "cpus" | "memory"] => "No limit".to_owned(),
        ["targets", target, "platform"] => {
            let kind = draft["targets"][target]["kind"].as_str().unwrap_or("");
            // A local engine runs this machine's architecture; a remote one
            // runs whatever its own host is, which setup cannot know.
            if matches!(kind, "local-podman" | "local-docker" | "apple-container") {
                format!("Engine default ({})", std::env::consts::ARCH)
            } else {
                "Engine default".to_owned()
            }
        }
        ["targets", _, "user"] => "From ~/.ssh/config, else current user".to_owned(),
        ["targets", _, "identity_file"] => "OpenSSH default keys".to_owned(),
        ["targets", _, "aws_profile"] => "AWS CLI default profile".to_owned(),
        ["targets", _, "launch_template_version"] => "$Default".to_owned(),
        // Until the target's host has been inspected, all the screen can say
        // is where the value comes from.
        ["targets", _, "build_cache", _] => "From the target host's mbx setup".to_owned(),
        ["phone", "tls_cert" | "tls_key"] => "None (HTTP only)".to_owned(),
        // A repository needs exactly one of these two, so a blank one is not
        // a default but a choice not yet made.
        ["bundles", _, "repositories", _, "github" | "local"] => "Not set".to_owned(),
        ["profiles", _, "context_window_bytes"] => format!(
            "{} ({} KiB)",
            mj_core::config::DEFAULT_CONTEXT_BYTES,
            mj_core::config::DEFAULT_CONTEXT_BYTES / 1024
        ),
        ["profiles", _, "guardian_review_model"] => {
            mj_core::config::GUARDIAN_REVIEW_NEWEST_FLASH.to_owned()
        }
        // Review needs a named reviewer profile; there is no fallback to a
        // first enabled profile (`ReviewConfig::reviewer_profile`).
        ["review", "profile"] => "No reviewer; reviews cannot run".to_owned(),
        ["review", "model" | "effort"] => match draft["review"]["profile"].as_str() {
            Some(profile) => format!("{profile}'s default"),
            None => "Not set".to_owned(),
        },
        _ => "Not set".to_owned(),
    }
}

pub(super) fn choice_label(path: &[String], value: &Value, draft: &Value) -> String {
    let Some(value) = value.as_str() else {
        return if value.is_null() {
            null_label(path, draft)
        } else {
            value.to_string()
        };
    };
    if let Ok(theme) =
        serde_json::from_value::<mj_core::config::UiTheme>(Value::String(value.into()))
    {
        return theme.label().into();
    }
    // An agent kind is named once, by the harness itself.
    if path.first().is_some_and(|key| key == "profiles")
        && let Ok(kind) =
            serde_json::from_value::<mj_core::config::HarnessKind>(Value::String(value.into()))
    {
        return kind.display_name().to_owned();
    }
    match value {
        "local-bare" => "Local worktree",
        "local-podman" => "Local Podman",
        "local-docker" => "Local Docker",
        "apple-container" => "Apple container",
        "ssh-bare" => "Remote SSH worktree",
        "ssh-podman" => "Remote SSH + Podman",
        "ssh-docker" => "Remote SSH + Docker",
        "aws-ec2" => "Amazon EC2",
        "guardian" => "Ask for approvals",
        "yolo" => "Allow all actions",
        "left" => "Left",
        "right" => "Right",
        "podman-volume" => "Managed volume",
        "container-layer" => "Inside the container",
        "host-helper" => "Custom storage helper",
        "quick" => "Quick",
        "extended" => "Extended",
        "public-dns" => "Public DNS name",
        "public-ip" => "Public IP address",
        "private-dns" => "Private DNS name",
        "private-ip" => "Private IP address",
        _ => value,
    }
    .to_owned()
}

pub(super) fn choices(path: &[String], draft: &Value) -> Vec<Value> {
    let key = path.last().map(String::as_str).unwrap_or("");
    let values: &[&str] = match key {
        "sessions_side" => &["left", "right"],
        "spinner" => &[], // Use the canonical animation list below.
        "tier" => &["quick", "extended"],
        "permissions" => &["guardian", "yolo"],
        "pull_policy" => &["auto", "always", "newer", "missing", "never"],
        "address_source" => &["public-dns", "public-ip", "private-dns", "private-ip"],
        "kind" if path.iter().any(|key| key == "workspace_storage") => {
            &["podman-volume", "host-helper", "container-layer"]
        }
        "kind" if path.first().is_some_and(|key| key == "profiles") => {
            &["codex", "claude", "kimi", "grok", "muse"]
        }
        "kind" => &[
            "local-bare",
            "local-podman",
            "local-docker",
            "apple-container",
            "ssh-bare",
            "ssh-podman",
            "ssh-docker",
            "aws-ec2",
        ],
        _ => &[],
    };
    if path.len() == 1 && key == "theme" {
        return mj_core::config::UiTheme::ALL
            .into_iter()
            .map(|theme| serde_json::to_value(theme).expect("theme serializes"))
            .collect();
    }
    if key == "spinner" {
        let first = mj_core::config::SpinnerStyle::default();
        let mut current = first;
        let mut choices = Vec::new();
        loop {
            choices.push(serde_json::to_value(current).expect("spinner serializes"));
            current = current.next();
            if current == first {
                return choices;
            }
        }
    }
    if key == "profile" {
        let mut values = vec![Value::Null];
        if let Some(entries) = draft["profiles"].as_object() {
            values.extend(entries.keys().cloned().map(Value::String));
        }
        return values;
    }
    values.iter().map(|v| Value::String((*v).into())).collect()
}

pub(super) fn help(path: &[String]) -> &'static str {
    match path.last().map(String::as_str).unwrap_or("") {
        "theme" => {
            "Colors for the terminal dashboard and conversation. Applies immediately after saving Settings."
        }
        "profiles" => {
            "Add an agent profile for each installed agent you want to run, or use Detect profiles to find the agents installed on this machine."
        }
        "home" => {
            "The agent's existing account directory, such as ~/.codex. ~ expands to your home when you apply. Sign in using the agent's own login command."
        }
        "targets" => {
            "Add a machine or runtime, or use Detect runtimes to find this machine's usable container runtimes. Choose its Type to see the settings it needs."
        }
        "phone" => {
            "Web access changes take effect when the background server next starts. Remote access requires a certificate and key."
        }
        "advanced" => "Optional diagnostics and display details for the activity surface.",
        "detailed_activity_clocks" => "Show elapsed turn and tool clocks in session activity rows.",
        "show_stopped_sessions" => "Include stopped sessions in the terminal Sessions pane.",
        "bundles" => {
            "Projects can contain one or more repositories. Choose the main repository where the agent starts."
        }
        "repositories" => {
            "Set either a local repository directory or a GitHub source for each repository."
        }
        "review" => {
            "Choose an agent profile for reviews. Model and effort can use the profile defaults."
        }
        "sessionwiki" => {
            "Your sessions are always indexed into SessionWiki so one search covers every coding tool; this section chooses archiving. The row below shows what it would free."
        }
        "archive_after_days" => {
            "Stopped sessions older than this many days lose their checkpoint and attachments once SessionWiki has indexed them; a fully merged branch goes too. Blank keeps all."
        }
        "subagents" => {
            "Enable Mjolnir-owned child agents and choose their concurrency limit and additional profiles. A parent profile is always eligible for its own children."
        }
        "eligible_profiles" => {
            "Check profiles that Claude and Codex parents may use in addition to their own profile."
        }
        "build_cache" => {
            "Share one mbx build cache between the Rust container sessions on each host. Leave the target settings blank to use that host's defaults."
        }
        "directory" => {
            "Cache directory on the target's own host. Blank uses that host's native mbx cache if mbx is installed there, otherwise ~/.cache/mbx."
        }
        "max_size" => {
            "Largest the cache may grow, such as 100GiB. Blank uses the host's own mbx limits, or min(100 GB, 1/4 of free space)."
        }
        "memory" => "Examples: 8g or 4096m. Leave blank for no limit.",
        "context_window_bytes" => {
            "Optional positive byte limit for transcript compaction. Leave blank for the default."
        }
        "guardian_review_model" => {
            "For a Codex profile with a custom model provider: newest-flash reviews with the newest flash model, session reviews with the session's own model, or name a model from the provider's catalog. Leave blank for newest-flash."
        }
        _ => "Enter opens or edits a setting. Changes stay in this draft until you save.",
    }
}

/// Path ownership is explicit: a similarly named environment key is still text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PathKind {
    Local,
    Target,
    RelativeDestination,
}

pub(super) fn path_kind(path: &[String]) -> Option<PathKind> {
    use PathKind::*;
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        ["profiles", _, "home"]
        | ["phone", "tls_cert" | "tls_key"]
        | ["targets", _, "identity_file"]
        | ["bundles", _, "repositories", _, "local"] => Some(Local),
        ["targets", _, "workspace_prefix"]
        | ["targets", _, "workspace_storage", "root"]
        | ["targets", _, "build_cache", "directory"] => Some(Target),
        ["bundles", _, "repositories", _, "destination"] => Some(RelativeDestination),
        _ => None,
    }
}
