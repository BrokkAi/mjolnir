//! Editable defaults and labels for every field in the user configuration.
use serde_json::{Value, json};

pub(super) fn defaults(path: &[String], value: &Value) -> Value {
    let key = path.last().map(String::as_str).unwrap_or("");
    match path.first().map(String::as_str).unwrap_or("") {
        "" => {
            json!({"sessions_side":"left", "spinner":"scan", "theme":"midnight", "advanced":{}, "notify":{}, "phone":{}, "review":{}, "sessionwiki":{}, "subagents":{}, "build_cache":{}, "profiles":{}, "machines":{}, "targets":{}, "bundles":{}})
        }
        "phone" => {
            json!({"enabled":true,"bind":"127.0.0.1:3765","tailscale_detect":true,"tls_cert":null,"tls_key":null})
        }
        "advanced" => {
            json!({"detailed_activity_clocks":false,"show_stopped_sessions":false,"session_order":"project","symbols":null})
        }
        "notify" => {
            json!({"mode":"terminal","bell":true,"delay_seconds":2,"title":true})
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
        "machines" if path.len() == 2 => machine_defaults(value["kind"].as_str().unwrap_or("ssh")),
        "targets" if path.len() == 2 => target_defaults(value["kind"].as_str().unwrap_or("podman")),
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

fn build_cache_defaults() -> Value {
    json!({"enabled":null,"directory":null,"max_size":null})
}

/// The fields one machine kind needs. A machine owns the host settings every
/// runtime on it shares, which is why the build cache lives here.
fn machine_defaults(kind: &str) -> Value {
    let mut fields = json!({"kind":kind});
    let object = fields.as_object_mut().unwrap();
    match kind {
        "ssh" => {
            object.extend(
                json!({"host":"","user":null,"identity_file":null,"extra_args":[],
                       "workspace_prefix":".local/share/hel/workspaces",
                       "build_cache":build_cache_defaults()})
                .as_object()
                .unwrap()
                .clone(),
            );
        }
        "aws-ec2" => {
            object.extend(json!({"aws_profile":null,"region":"","launch_template":"","launch_template_version":null,"ssh_user":"ubuntu","address_source":"public-dns","identity_file":null,"ssh_args":[]}).as_object().unwrap().clone());
        }
        // This machine, which has nothing to connect to.
        _ => {
            object.insert("build_cache".to_owned(), build_cache_defaults());
        }
    }
    fields
}

/// The fields one runtime kind needs. Every runtime names the machine it runs
/// on; the host settings are on that machine, not here.
fn target_defaults(kind: &str) -> Value {
    let mut fields = json!({"kind":kind, "machine":mj_core::config::LOCAL_MACHINE_ID});
    if kind == "bare" {
        fields
            .as_object_mut()
            .unwrap()
            .insert("permissions".to_owned(), Value::Null);
    } else {
        fields.as_object_mut().unwrap().extend(json!({
            "image":mj_client::target::DEFAULT_IMAGE,"pull_policy":"auto","platform":null,
            "cpus":null,"memory":null,"environment":{},"workspace_storage":{"kind":"podman-volume"}
        }).as_object().unwrap().clone());
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

/// Whether the runtime `target` runs on this machine, which is what decides
/// the settings the screen can resolve without asking another host.
pub(super) fn is_local_runtime(draft: &Value, target: &str) -> bool {
    draft["targets"][target]["machine"]
        .as_str()
        .unwrap_or(mj_core::config::LOCAL_MACHINE_ID)
        == mj_core::config::LOCAL_MACHINE_ID
}

pub(super) fn label(key: &str) -> String {
    match key {
        "interface" => "Interface",
        "sessions_side" => "Session sidebar position",
        "spinner" => "Activity animation",
        "advanced" => "Advanced",
        "detailed_activity_clocks" => "Detailed activity clocks",
        "show_stopped_sessions" => "Show stopped sessions",
        "session_order" => "Session order",
        "symbols" => "Symbols",
        "notify" => "Notifications",
        "mode" => "Notify through",
        "bell" => "Ring the terminal bell",
        "delay_seconds" => "Delay before notifying (seconds)",
        "title" => "Show counts in the terminal title",
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
        "machines" => "Machines",
        "targets" => "Runtimes",
        "machine" => "Machine",
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
        "workspace_prefix" => "Workspace directory for bare runtimes",
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
        // Unset symbols follow the terminal: ASCII on the Linux console or
        // without a UTF-8 locale, Unicode otherwise.
        ["advanced", "symbols"] => "Follows the terminal".to_owned(),
        // The backend emits no CPU or memory flag, so the container competes
        // for the whole machine.
        ["targets", _, "cpus" | "memory"] => "No limit".to_owned(),
        ["targets", target, "platform"] => {
            // A runtime on this machine runs this machine's architecture; one
            // on another machine runs whatever that host is, which setup
            // cannot know.
            if is_local_runtime(draft, target) {
                format!("Engine default ({})", std::env::consts::ARCH)
            } else {
                "Engine default".to_owned()
            }
        }
        // A bare runtime on an SSH machine takes the guardian default.
        ["targets", _, "permissions"] => "Ask for approvals".to_owned(),
        ["machines", _, "user"] => "From ~/.ssh/config, else current user".to_owned(),
        ["machines", _, "identity_file"] => "OpenSSH default keys".to_owned(),
        ["machines", _, "aws_profile"] => "AWS CLI default profile".to_owned(),
        ["machines", _, "launch_template_version"] => "$Default".to_owned(),
        // Until the machine has been inspected, all the screen can say is
        // where the value comes from.
        ["machines", _, "build_cache", _] => "From the machine's own mbx setup".to_owned(),
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

/// What a section of the first page is actually set to.
///
/// The first page lists sections rather than values, and a count of what is
/// inside one answers no question anyone has: what a reader wants from
/// "Web Access" is whether it is on and where it listens. `None` leaves a
/// section to the ordinary value summary.
pub(super) fn section_summary(key: &str, draft: &Value) -> Option<String> {
    let section = &draft[key];
    Some(match key {
        "interface" => format!(
            "{} · sidebar {}",
            choice_label(&["theme".to_owned()], &draft["theme"], draft),
            choice_label(
                &["sessions_side".to_owned()],
                &draft["sessions_side"],
                draft
            )
            .to_lowercase()
        ),
        "advanced" => {
            let on = ["detailed_activity_clocks", "show_stopped_sessions"]
                .iter()
                .filter(|field| section[*field] == Value::Bool(true))
                .count();
            let order = if section["session_order"] == Value::String("priority".to_owned()) {
                " · priority order"
            } else {
                ""
            };
            match on {
                0 => format!("All off{order}"),
                count => format!("{count} on{order}"),
            }
        }
        "notify" => match section["mode"].as_str() {
            Some("off") => "Off".to_owned(),
            Some("system") => "Desktop and terminal".to_owned(),
            _ => "Terminal".to_owned(),
        },
        "profiles" | "machines" | "targets" | "bundles" => named_entries(section),
        "phone" => {
            if section["enabled"] == Value::Bool(false) {
                "Off".to_owned()
            } else {
                match section["bind"].as_str() {
                    Some(bind) => format!("On · {bind}"),
                    None => "On".to_owned(),
                }
            }
        }
        "review" => {
            if section["enabled"] != Value::Bool(true) {
                "Off".to_owned()
            } else {
                let tier = choice_label(&["tier".to_owned()], &section["tier"], draft);
                match section["profile"].as_str() {
                    Some(profile) => format!("{tier} · {profile}"),
                    None => format!("{tier} · no reviewer"),
                }
            }
        }
        "subagents" => {
            if section["enabled"] == Value::Bool(false) {
                "Off".to_owned()
            } else {
                match section["max_concurrent"].as_u64() {
                    Some(limit) => format!("On · up to {limit}"),
                    None => "On".to_owned(),
                }
            }
        }
        "sessionwiki" => match section["archive_after_days"].as_u64() {
            Some(days) => format!("Archives after {days} days"),
            None => "Keeps every session".to_owned(),
        },
        "build_cache" => {
            if section["enabled"].as_bool().unwrap_or(true) {
                "Shared".to_owned()
            } else {
                "Off".to_owned()
            }
        }
        _ => return None,
    })
}

/// The entries of a section the user names, listed rather than counted. Past
/// three the tail becomes a count, so the column stays one line.
fn named_entries(value: &Value) -> String {
    let Some(entries) = value.as_object().filter(|entries| !entries.is_empty()) else {
        return "None yet".to_owned();
    };
    let names = entries.keys().map(String::as_str).collect::<Vec<_>>();
    if names.len() <= 3 {
        return names.join(", ");
    }
    format!("{}, +{} more", names[..2].join(", "), names.len() - 2)
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
    // A download policy is shown by what it does to this target's own image:
    // `auto` is derived from the image reference, not an alias.
    if let ["targets", target, "pull_policy"] = path
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
        && let Ok(policy) =
            serde_json::from_value::<mj_core::config::ImagePullPolicy>(Value::String(value.into()))
    {
        let image = draft["targets"][target]["image"].as_str().unwrap_or("");
        return policy.describe(image).to_owned();
    }
    // An agent kind is named once, by the harness itself.
    if path.first().is_some_and(|key| key == "profiles")
        && let Ok(kind) =
            serde_json::from_value::<mj_core::config::HarnessKind>(Value::String(value.into()))
    {
        return kind.display_name().to_owned();
    }
    // A machine id is shown as the user named it.
    if path.last().is_some_and(|key| key == "machine") {
        return value.to_owned();
    }
    match value {
        "local" => "This machine",
        "ssh" => "SSH host",
        "aws-ec2" => "Amazon EC2",
        "bare" => "Bare checkout",
        "podman" => "Podman",
        "docker" => "Docker",
        "apple-container" => "Apple container",
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
        "session_order" => &["project", "priority"],
        "symbols" => &["unicode", "ascii"],
        "mode" if path.first().is_some_and(|key| key == "notify") => &["off", "terminal", "system"],
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
        "kind" if path.first().is_some_and(|key| key == "machines") => &["local", "ssh", "aws-ec2"],
        "kind" => &["bare", "podman", "docker", "apple-container"],
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
    if key == "machine" {
        let mut values = vec![Value::String(mj_core::config::LOCAL_MACHINE_ID.to_owned())];
        if let Some(entries) = draft["machines"].as_object() {
            values.extend(
                entries
                    .keys()
                    .filter(|id| id.as_str() != mj_core::config::LOCAL_MACHINE_ID)
                    .cloned()
                    .map(Value::String),
            );
        }
        return values;
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
        "machines" => {
            "Add an SSH host or an EC2 launch template. This machine is always listed as local. Build cache settings live here because every runtime on a machine shares them."
        }
        "targets" => {
            "Add a runtime and choose the machine it runs on, or use Detect runtimes to find this machine's usable container engines."
        }
        "machine" => "Which machine this runtime runs on.",
        "phone" => {
            "Web access changes take effect when the background server next starts. Remote access requires a certificate and key."
        }
        "advanced" => "Optional diagnostics and display details for the activity surface.",
        "notify" => {
            "How to be told when a session you are not looking at asks a question, fails, or finishes."
        }
        "mode" if path.first().is_some_and(|key| key == "notify") => {
            "Terminal rings the bell and works over SSH. System also posts a desktop notification through osascript or notify-send."
        }
        "bell" => "Ring the terminal bell with each notification.",
        "delay_seconds" => {
            "Wait this long before notifying, so a question the agent answers itself stays quiet."
        }
        "title" => {
            "Keep the terminal window title showing how many sessions are waiting or unread."
        }
        "detailed_activity_clocks" => "Show elapsed turn and tool clocks in session activity rows.",
        "show_stopped_sessions" => "Include stopped sessions in the terminal Sessions pane.",
        "session_order" => {
            "Group sessions by project, or list the ones that need you first without project headings."
        }
        "symbols" => {
            "Draw status marks, borders, and separators with Unicode or plain ASCII. Unset, the terminal decides: ASCII on the Linux console or without a UTF-8 locale."
        }
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
            "Share one mbx build cache between the Rust container sessions on each machine. Leave a machine's settings blank to use its own defaults."
        }
        "directory" => {
            "Cache directory on the machine itself. Blank uses that machine's native mbx cache if mbx is installed there, otherwise ~/.cache/mbx."
        }
        "max_size" => {
            "Largest the cache may grow, such as 100GiB. Blank uses the host's own mbx limits, or min(100 GB, 1/4 of free space)."
        }
        "memory" => "Examples: 8g or 4096m. Leave blank for no limit.",
        "pull_policy" => {
            "When Mjolnir downloads this image. The first choice is derived from the image: it never delays a launch, pulling only a missing image, while the daemon refreshes a remote :latest image in the background."
        }
        "context_window_bytes" => {
            "Optional positive byte limit for transcript compaction. Leave blank for the default."
        }
        "guardian_review_model" => {
            "For a Codex profile with a custom model provider: newest-flash reviews with the newest flash model, session reviews with the session's own model, or name a model from the provider's catalog. Leave blank for newest-flash."
        }
        // The first page has no parent setting to describe, so it says what
        // the whole screen does instead.
        "" => "Changes stay in this draft until you save.",
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

/// What a local path setting accepts. Three settings name a file; every other
/// path setting names a directory.
pub(super) fn completion_kind(path: &[String]) -> mj_core::path_completion::CompletionKind {
    use mj_core::path_completion::CompletionKind;
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        ["phone", "tls_cert" | "tls_key"] | ["machines", _, "identity_file"] => CompletionKind::Any,
        _ => CompletionKind::Directories,
    }
}

pub(super) fn path_kind(path: &[String]) -> Option<PathKind> {
    use PathKind::*;
    let parts = path.iter().map(String::as_str).collect::<Vec<_>>();
    match parts.as_slice() {
        ["profiles", _, "home"]
        | ["phone", "tls_cert" | "tls_key"]
        | ["machines", _, "identity_file"]
        | ["bundles", _, "repositories", _, "local"] => Some(Local),
        ["machines", _, "workspace_prefix"]
        | ["machines", _, "build_cache", "directory"]
        | ["targets", _, "workspace_storage", "root"] => Some(Target),
        ["bundles", _, "repositories", _, "destination"] => Some(RelativeDestination),
        _ => None,
    }
}
