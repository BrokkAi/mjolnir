//! Transcript projection and presentation implementation.

use agent_client_protocol::schema::v1::{
    SessionUpdate, ToolCall, ToolCallContent, ToolCallLocation, ToolCallUpdateFields, ToolKind,
};
use mj_core::acp::RuntimeEvent;
pub use mj_core::transcript::*;
use serde::Deserialize;
use serde_json::Value;
use tree_sitter::{Node, Parser};
const TOOL_SUMMARY_SOURCE_BYTES: usize = 64 * 1024;
/// Parser-rule version stored with cached tool summaries.
pub const TOOL_SUMMARY_VERSION: u8 = 2;

/// Reduce a tool call to what a reader still needs, once a verified checkpoint
/// holds the whole of it.
///
/// Tool output is where a projection's bytes are: on one measured session,
/// 561 MB of 635 MiB. Behind a checkpoint nothing reads it — the checkpoint
/// archive carries the complete transcript, and restoring it brings the output
/// back — so what stays here is what the transcript still shows: which tool
/// ran, on what, with what result, and how many lines each edit changed.
///
/// Returns whether anything changed, so a caller can skip the write.
pub fn compact_tool_call_for_retention(body: &mut TranscriptBody) -> bool {
    let TranscriptBody::Tool {
        call,
        terminal_outputs,
        terminal_refs,
        ..
    } = body
    else {
        return false;
    };
    let Some(object) = call.as_object_mut() else {
        return false;
    };
    let mut changed = !terminal_outputs.is_empty() || !terminal_refs.is_empty();
    terminal_outputs.clear();
    terminal_refs.clear();
    for field in ["rawInput", "rawOutput", "_meta"] {
        changed |= object.remove(field).is_some();
    }
    let Some(content) = object
        .get_mut("content")
        .and_then(|value| value.as_array_mut())
    else {
        return changed;
    };
    let before = content.len();
    // Diffs stay, because the transcript still shows their stat. Their patch
    // text does not, and neither do the two file copies an older record holds
    // instead of a patch: `diff::drop_patch_text` turns those into the
    // counts `format_diffstat` reads before dropping them.
    content.retain(|item| item.get("type").and_then(|kind| kind.as_str()) == Some("diff"));
    changed |= content.len() != before;
    for item in content.iter_mut() {
        changed |= drop_diff_body(item);
    }
    changed
}

fn drop_diff_body(item: &mut serde_json::Value) -> bool {
    use agent_client_protocol::schema::v1::ToolCallContent;

    // Round-trip through `ToolCallContent`, not `Diff`: the variant tag lives
    // on the enum, and writing back a bare `Diff` would strip it and make the
    // whole tool call unreadable.
    let mut content = match serde_json::from_value::<ToolCallContent>(item.clone()) {
        Ok(content) => content,
        // Content this cannot read is content it must not rewrite.
        Err(error) => {
            tracing::warn!(%error, "skipping unreadable tool content during retention");
            return false;
        }
    };
    let ToolCallContent::Diff(diff) = &mut content else {
        return false;
    };
    if !mj_core::diff::drop_patch_text(diff) {
        return false;
    }
    match serde_json::to_value(&content) {
        Ok(value) => {
            *item = value;
            true
        }
        Err(error) => {
            tracing::warn!(%error, "could not rewrite a diff during retention");
            false
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolSummarySource {
    Shell(String),
    Argv {
        executable: String,
        arguments: Vec<String>,
    },
}

/// Whether a partial update changes inputs used to derive the tool summary.
pub fn tool_call_update_changes_presentation(
    call: &ToolCall,
    fields: &ToolCallUpdateFields,
) -> bool {
    fields
        .title
        .as_ref()
        .is_some_and(|title| title != &call.title)
        || fields.kind.is_some_and(|kind| kind != call.kind)
        || fields
            .raw_input
            .as_ref()
            .is_some_and(|input| Some(input) != call.raw_input.as_ref())
        || (call.kind == ToolKind::Execute
            && fields
                .raw_output
                .as_ref()
                .is_some_and(|output| Some(output) != call.raw_output.as_ref())
            && command_source(call.raw_input.as_ref()).is_none())
}

/// Compute the stable presentation metadata for one complete ACP call.
pub fn tool_call_presentation(call: &ToolCall) -> ToolCallPresentation {
    let kind = call.kind;
    if kind == ToolKind::Execute {
        if let Some(source) = command_source(call.raw_input.as_ref()) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawInput,
                kind,
                &call.title,
            );
        }
        if let Some(source) = command_source(call.raw_output.as_ref()) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawOutput,
                kind,
                &call.title,
            );
        }
    }
    presentation_from_title(&call.title, kind)
}

/// Use cached presentation data when it was produced by current parser rules,
/// otherwise rebuild it from the complete stored call. This lets parser fixes
/// repair existing transcripts while current summaries remain cheap to load.
pub fn materialized_tool_call_presentation(
    stored: Option<&ToolCallPresentation>,
    call: &ToolCall,
) -> ToolCallPresentation {
    stored
        .filter(|presentation| presentation.summary_version >= TOOL_SUMMARY_VERSION)
        .cloned()
        .unwrap_or_else(|| tool_call_presentation(call))
}

/// Apply the presentation-relevant portion of a partial ACP update to cached
/// metadata. ACP updates replace only fields that are present, so a title
/// update must not erase a summary selected from an earlier raw command.
pub fn update_tool_call_presentation(
    previous: Option<&ToolCallPresentation>,
    title: &str,
    kind: Option<ToolKind>,
    raw_input: Option<&Value>,
    raw_output: Option<&Value>,
) -> ToolCallPresentation {
    let next_kind = kind.unwrap_or_else(|| {
        previous
            .map(|presentation| presentation.tool_kind)
            .unwrap_or_default()
    });

    if next_kind == ToolKind::Execute {
        if let Some(source) = raw_input.and_then(|value| command_source(Some(value))) {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawInput,
                next_kind,
                title,
            );
        }
        if let Some(source) = raw_output.and_then(|value| command_source(Some(value)))
            && (raw_input.is_some()
                || !previous.is_some_and(|previous| {
                    previous.source_kind == ToolSummarySourceKind::RawInput
                }))
        {
            return presentation_from_source(
                source,
                ToolSummarySourceKind::RawOutput,
                next_kind,
                title,
            );
        }
        if let Some(previous) = previous
            && previous.tool_kind == ToolKind::Execute
            && ((raw_input.is_none() && previous.source_kind == ToolSummarySourceKind::RawInput)
                || (raw_input.is_none()
                    && raw_output.is_none()
                    && previous.source_kind == ToolSummarySourceKind::RawOutput))
        {
            return ToolCallPresentation {
                tool_kind: next_kind,
                ..previous.clone()
            };
        }
    }

    presentation_from_title(title, next_kind)
}

fn command_source(raw: Option<&Value>) -> Option<ToolSummarySource> {
    let command = raw?.get("command")?;
    match command {
        Value::String(command) if !command.trim().is_empty() => {
            Some(ToolSummarySource::Shell(command.clone()))
        }
        Value::Array(argv) => {
            let argv = argv.iter().map(Value::as_str).collect::<Option<Vec<_>>>()?;
            let first = argv.first()?.trim();
            if first.is_empty() {
                return None;
            }
            if is_shell_interpreter(first)
                && let Some(script) = shell_script_argument(&argv[1..])
            {
                return Some(ToolSummarySource::Shell(script.to_owned()));
            }
            Some(ToolSummarySource::Argv {
                executable: first.to_owned(),
                arguments: argv[1..]
                    .iter()
                    .map(|argument| (*argument).to_owned())
                    .collect(),
            })
        }
        _ => None,
    }
}

fn is_shell_interpreter(value: &str) -> bool {
    let executable = value.rsplit('/').next().unwrap_or(value);
    matches!(executable, "sh" | "bash" | "dash" | "zsh")
}

fn shell_script_argument<'a>(arguments: &'a [&'a str]) -> Option<&'a str> {
    let mut index = 0;
    while index < arguments.len() {
        let argument = arguments[index];
        if argument == "--" {
            return None;
        }
        if argument == "-c" || argument == "--command" {
            return arguments.get(index + 1).copied();
        }
        if argument.starts_with('-') && !argument.starts_with("--") && argument[1..].contains('c') {
            return arguments.get(index + 1).copied();
        }
        index += 1;
    }
    None
}

fn presentation_from_source(
    source: ToolSummarySource,
    source_kind: ToolSummarySourceKind,
    tool_kind: ToolKind,
    title: &str,
) -> ToolCallPresentation {
    let (source, summary) = match source {
        ToolSummarySource::Shell(source) => {
            let bounded = bound_summary_source(&source);
            let summary = summarize_shell(&bounded)
                .or_else(|| first_meaningful_token(title))
                .unwrap_or_else(|| "tool".to_owned());
            (bounded, summary)
        }
        ToolSummarySource::Argv {
            executable,
            arguments,
        } => {
            let source = std::iter::once(executable.as_str())
                .chain(arguments.iter().map(String::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            let bounded = bound_summary_source(&source);
            let summary = summarize_invocation(&executable, &arguments)
                .or_else(|| first_meaningful_token(title))
                .unwrap_or_else(|| "tool".to_owned());
            (bounded, summary)
        }
    };
    ToolCallPresentation {
        summary,
        source,
        source_kind,
        tool_kind,
        summary_version: TOOL_SUMMARY_VERSION,
    }
}

fn presentation_from_title(title: &str, tool_kind: ToolKind) -> ToolCallPresentation {
    let source = title_source(title);
    let bounded = bound_summary_source(&source);
    let summary = if tool_kind == ToolKind::Execute {
        summarize_shell(&bounded)
            .or_else(|| first_meaningful_token(&bounded))
            .unwrap_or_else(|| "tool".to_owned())
    } else if bounded.trim_end().ends_with('?') {
        // A question tool (Claude's AskUserQuestion) is titled with the
        // question; its first word alone ("Which") says nothing.
        bounded.trim().to_owned()
    } else {
        first_meaningful_token(&bounded).unwrap_or_else(|| "tool".to_owned())
    };
    ToolCallPresentation {
        summary,
        source: bounded,
        source_kind: ToolSummarySourceKind::Title,
        tool_kind,
        summary_version: TOOL_SUMMARY_VERSION,
    }
}

fn title_source(title: &str) -> String {
    let title = title.trim();
    let title = title
        .strip_prefix("Running:")
        .or_else(|| title.strip_prefix("Starting background:"))
        .map(str::trim)
        .unwrap_or(title);
    if let Some(inner) = title
        .strip_prefix("Execute `")
        .and_then(|value| value.strip_suffix('`'))
    {
        return inner.to_owned();
    }
    title.to_owned()
}

fn first_meaningful_token(value: &str) -> Option<String> {
    let token = value
        .split_whitespace()
        .next()?
        .trim_matches(|character: char| {
            !character.is_alphanumeric() && character != '/' && character != '.' && character != '_'
        });
    if token.is_empty() {
        None
    } else {
        Some(token.trim_matches(['\'', '"', '`']).to_owned())
    }
}

fn bound_summary_source(source: &str) -> String {
    if source.len() <= TOOL_SUMMARY_SOURCE_BYTES {
        return source.to_owned();
    }
    let mut end = TOOL_SUMMARY_SOURCE_BYTES;
    while !source.is_char_boundary(end) {
        end -= 1;
    }
    source[..end].to_owned()
}

fn summarize_shell(source: &str) -> Option<String> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .ok()?;
    let tree = parser.parse(source, None)?;
    let root = tree.root_node();
    if root.has_error() {
        return None;
    }

    let mut commands = Vec::new();
    let mut operators = Vec::new();
    let mut subshells = Vec::new();
    if !collect_shell_tokens(root, source, &mut commands, &mut operators, &mut subshells) {
        return None;
    }
    if commands.is_empty() {
        return None;
    }
    commands.sort_by_key(|command| command.start);
    operators.sort_by_key(|operator| operator.start);
    subshells.sort_by_key(|subshell| subshell.start);

    let mut tokens = Vec::new();
    for (index, command) in commands.iter().enumerate() {
        if index > 0 {
            let previous = &commands[index - 1];
            let separator = shell_separator_between(previous, command, &operators);
            tokens.push(ShellToken {
                start: separator.start,
                text: separator.kind,
                order: 1,
            });
        }
        tokens.push(ShellToken {
            start: command.start,
            text: command.summary.clone(),
            order: 2,
        });
    }

    // Parentheses are meaningful only for subshells that contain a command we
    // retained. Other punctuation, such as case arms and group delimiters,
    // is structural and must not leak into the compact summary.
    for subshell in subshells {
        if !commands
            .iter()
            .any(|command| command.start >= subshell.start && command.end <= subshell.end)
        {
            continue;
        }
        let close = subshell.end.saturating_sub(1);
        tokens.push(ShellToken {
            start: subshell.start,
            text: "(".to_owned(),
            order: 0,
        });
        tokens.push(ShellToken {
            start: close,
            text: ")".to_owned(),
            order: 3,
        });
    }

    tokens.sort_by_key(|token| (token.start, token.order));
    Some(join_shell_tokens(
        tokens.into_iter().map(|token| token.text).collect(),
    ))
}

#[derive(Debug, Clone)]
struct ShellCommandToken {
    start: usize,
    end: usize,
    summary: String,
}

#[derive(Debug, Clone)]
struct ShellOperatorToken {
    start: usize,
    kind: String,
}

#[derive(Debug, Clone)]
struct ShellSubshell {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone)]
struct ShellToken {
    start: usize,
    text: String,
    order: u8,
}

fn collect_shell_tokens(
    node: Node<'_>,
    source: &str,
    commands: &mut Vec<ShellCommandToken>,
    operators: &mut Vec<ShellOperatorToken>,
    subshells: &mut Vec<ShellSubshell>,
) -> bool {
    let kind = node.kind();
    if matches!(kind, "command_substitution" | "process_substitution") {
        return true;
    }
    if kind == "command" {
        if let Some(summary) = summarize_command_node(node, source) {
            commands.push(ShellCommandToken {
                start: node.start_byte(),
                end: node.end_byte(),
                summary,
            });
            return true;
        }
        return false;
    }
    if is_shell_operator(node) {
        operators.push(ShellOperatorToken {
            start: node.start_byte(),
            kind: kind.to_owned(),
        });
        return true;
    }

    if kind == "subshell" {
        subshells.push(ShellSubshell {
            start: node.start_byte(),
            end: node.end_byte(),
        });
    }

    let mut cursor = node.walk();
    node.children(&mut cursor)
        .all(|child| collect_shell_tokens(child, source, commands, operators, subshells))
}

fn shell_separator_between(
    previous: &ShellCommandToken,
    next: &ShellCommandToken,
    operators: &[ShellOperatorToken],
) -> ShellOperatorToken {
    let mut candidates = operators
        .iter()
        .filter(|operator| operator.start >= previous.end && operator.start < next.start);
    let structural = candidates.clone().find(|operator| operator.kind != ";");
    if let Some(operator) = structural {
        return operator.clone();
    }
    if let Some(operator) = candidates.find(|operator| operator.kind == ";") {
        return operator.clone();
    }
    ShellOperatorToken {
        start: previous.end,
        kind: ";".to_owned(),
    }
}

#[derive(Debug, Clone)]
struct InvocationArgument {
    value: String,
    literal: bool,
}

fn summarize_command_node(node: Node<'_>, source: &str) -> Option<String> {
    let name = node.child_by_field_name("name")?;
    let executable = shell_command_name(name, source)?;
    let mut cursor = node.walk();
    let arguments = node
        .children_by_field_name("argument", &mut cursor)
        .map(|argument| {
            let value = shell_argument_value(argument, source);
            InvocationArgument {
                value: value.clone().unwrap_or_default(),
                literal: value.is_some(),
            }
        })
        .collect::<Vec<_>>();
    summarize_invocation_with_literals(&executable, &arguments)
}

fn shell_command_name(node: Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_shell_node(node) {
        return None;
    }
    normalize_command_name(&source[node.byte_range()])
}

fn shell_argument_value(node: Node<'_>, source: &str) -> Option<String> {
    if contains_dynamic_shell_node(node) {
        return None;
    }
    let text = source[node.byte_range()].trim();
    if text.is_empty() {
        return None;
    }
    Some(strip_matching_quotes(text).to_owned())
}

fn contains_dynamic_shell_node(node: Node<'_>) -> bool {
    if matches!(
        node.kind(),
        "expansion"
            | "simple_expansion"
            | "command_substitution"
            | "process_substitution"
            | "arithmetic_expansion"
    ) {
        return true;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).any(contains_dynamic_shell_node)
}

fn summarize_invocation(executable: &str, arguments: &[String]) -> Option<String> {
    summarize_invocation_with_literals(
        executable,
        &arguments
            .iter()
            .map(|value| InvocationArgument {
                value: strip_matching_quotes(value).to_owned(),
                literal: true,
            })
            .collect::<Vec<_>>(),
    )
}

fn summarize_invocation_with_literals(
    executable: &str,
    arguments: &[InvocationArgument],
) -> Option<String> {
    let executable = normalize_command_name(executable)?;
    let basename = executable
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(&executable)
        .to_owned();
    let mut words = vec![executable];
    if !is_summary_executable(&basename) {
        return Some(words.remove(0));
    }
    let mut index = 0;

    if basename == "cargo"
        && arguments.first().is_some_and(|argument| {
            argument.literal && argument.value.starts_with('+') && argument.value.len() > 1
        })
    {
        index += 1;
    }

    let first_verb = loop {
        let Some(argument) = arguments.get(index) else {
            return Some(words.remove(0));
        };
        if !argument.literal {
            return Some(words.remove(0));
        }
        if argument.value == "--" || argument.value.starts_with('-') {
            if let Some(consumed) = known_leading_option_arguments(&basename, arguments, index) {
                index += consumed;
                continue;
            }
            return Some(words.remove(0));
        }
        break argument.value.clone();
    };
    words.push(first_verb.clone());

    if allows_second_verb(&basename, &first_verb) {
        let first = index + 1;
        if let Some(argument) = arguments.get(first)
            && argument.literal
            && !argument.value.starts_with('-')
            && argument.value != "--"
        {
            words.push(argument.value.clone());
        }
    }
    Some(words.join(" "))
}

fn is_summary_executable(basename: &str) -> bool {
    matches!(
        basename,
        "git"
            | "gh"
            | "cargo"
            | "rustup"
            | "npm"
            | "pnpm"
            | "yarn"
            | "bun"
            | "uv"
            | "pip"
            | "pip3"
            | "docker"
            | "podman"
            | "nice"
    )
}

fn allows_second_verb(basename: &str, first_verb: &str) -> bool {
    match basename {
        "gh" => matches!(
            first_verb,
            "alias"
                | "auth"
                | "cache"
                | "codespace"
                | "config"
                | "extension"
                | "gist"
                | "gpg-key"
                | "issue"
                | "label"
                | "org"
                | "pr"
                | "project"
                | "release"
                | "repo"
                | "ruleset"
                | "run"
                | "search"
                | "secret"
                | "ssh-key"
                | "variable"
                | "workflow"
        ),
        "docker" => matches!(
            first_verb,
            "buildx"
                | "compose"
                | "config"
                | "context"
                | "container"
                | "image"
                | "manifest"
                | "network"
                | "node"
                | "plugin"
                | "secret"
                | "service"
                | "stack"
                | "swarm"
                | "system"
                | "trust"
                | "volume"
        ),
        "podman" => matches!(
            first_verb,
            "artifact"
                | "container"
                | "farm"
                | "generate"
                | "image"
                | "machine"
                | "manifest"
                | "network"
                | "play"
                | "pod"
                | "secret"
                | "system"
                | "volume"
        ),
        "uv" => matches!(first_verb, "cache" | "pip" | "python" | "tool"),
        "rustup" => matches!(
            first_verb,
            "component" | "override" | "target" | "toolchain"
        ),
        _ => false,
    }
}

fn known_leading_option_arguments(
    basename: &str,
    arguments: &[InvocationArgument],
    index: usize,
) -> Option<usize> {
    let option = arguments.get(index)?.value.as_str();
    if option == "--" {
        if basename == "nice" {
            return Some(1);
        }
        return None;
    }
    let (option_name, attached_value) = option
        .split_once('=')
        .map_or((option, false), |(name, _)| (name, true));
    if basename == "nice"
        && option.starts_with('-')
        && option.len() > 1
        && option[1..].parse::<i32>().is_ok()
    {
        return Some(1);
    }
    if basename == "nice"
        && option
            .strip_prefix("-n")
            .is_some_and(|value| !value.is_empty() && value.parse::<i32>().is_ok())
    {
        return Some(1);
    }
    let attached_short_value = match basename {
        "git" => option.starts_with("-C") || option.starts_with("-c"),
        "gh" => option.starts_with("-R"),
        "docker" | "podman" => option.starts_with("-H"),
        _ => false,
    } && option.len() > 2;
    let takes_value = match basename {
        "git" => matches!(
            option_name,
            "-C" | "-c"
                | "--config-env"
                | "--exec-path"
                | "--git-dir"
                | "--namespace"
                | "--super-prefix"
                | "--work-tree"
        ),
        "gh" => matches!(
            option_name,
            "-R" | "--hostname" | "--repo" | "--jq" | "--template"
        ),
        "cargo" => matches!(
            option_name,
            "--manifest-path" | "--target-dir" | "--config" | "--color"
        ),
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(
                option_name,
                "--cwd" | "--dir" | "--prefix" | "--registry" | "--userconfig"
            )
        }
        "uv" => matches!(option_name, "--directory" | "--project" | "--python"),
        "rustup" => matches!(option_name, "--toolchain"),
        "nice" => {
            matches!(option_name, "-n" | "--adjustment")
        }
        "docker" | "podman" => matches!(
            option_name,
            "-H" | "--config" | "--connection" | "--context" | "--host" | "--log-level"
        ),
        _ => false,
    };
    if attached_short_value {
        return Some(1);
    }
    if attached_value {
        return takes_value.then_some(1);
    }
    if takes_value {
        return arguments
            .get(index + 1)
            .filter(|argument| argument.literal)
            .map(|_| 2);
    }
    let known_flag = match basename {
        "git" => matches!(
            option_name,
            "-p" | "--paginate"
                | "-P"
                | "--no-pager"
                | "--bare"
                | "--literal-pathspecs"
                | "--glob-pathspecs"
                | "--noglob-pathspecs"
                | "--icase-pathspecs"
                | "--no-optional-locks"
                | "--no-advice"
        ),
        "gh" => false,
        "cargo" => matches!(
            option_name,
            "-q" | "--quiet" | "-v" | "--verbose" | "--locked" | "--offline" | "--frozen"
        ),
        "npm" | "pnpm" | "yarn" | "bun" => {
            matches!(option_name, "-g" | "--global" | "--silent")
        }
        "uv" => matches!(
            option_name,
            "-q" | "--quiet" | "-v" | "--verbose" | "--offline"
        ),
        "rustup" => matches!(option_name, "-q" | "--quiet" | "-v" | "--verbose"),
        "docker" | "podman" => matches!(option_name, "-D" | "--debug" | "--tls"),
        "nice" => false,
        _ => false,
    };
    known_flag.then_some(1)
}

fn strip_matching_quotes(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn is_shell_operator(node: Node<'_>) -> bool {
    match node.kind() {
        ";" => true,
        "&&" | "||" => node.parent().is_some_and(|parent| parent.kind() == "list"),
        "|" | "|&" => node
            .parent()
            .is_some_and(|parent| parent.kind() == "pipeline"),
        "&" => node.parent().is_none_or(|parent| {
            !matches!(
                parent.kind(),
                "binary_expression" | "unary_expression" | "postfix_expression"
            )
        }),
        _ => false,
    }
}

fn normalize_command_name(text: &str) -> Option<String> {
    let text = text.trim();
    if text.contains('$') || text.contains('`') {
        return None;
    }
    let text = text
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            text.strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(text);
    (!text.is_empty()).then(|| text.to_owned())
}

fn join_shell_tokens(tokens: Vec<String>) -> String {
    let mut output = String::new();
    for token in tokens {
        match token.as_str() {
            "(" => {
                if !output.is_empty() && !output.ends_with(' ') {
                    output.push(' ');
                }
                output.push('(');
            }
            ")" => {
                output = output.trim_end().to_owned();
                output.push(')');
            }
            _ => {
                if !output.is_empty() && !output.ends_with(' ') && !output.ends_with('(') {
                    output.push(' ');
                }
                output.push_str(&token);
            }
        }
    }
    output
}

pub fn tool_content_details(
    content: &[ToolCallContent],
    terminal_outputs: &[TerminalOutputRecord],
    raw_output: Option<&serde_json::Value>,
) -> Vec<String> {
    let mut details = Vec::new();
    let mut referenced: Vec<&str> = Vec::new();
    for item in content {
        let detail = match item {
            ToolCallContent::Content(content) => content_block_text(&content.content),
            ToolCallContent::Diff(_) => None,
            // Kimi-style agents send a terminal reference and no textual copy
            // of the output, so the record hel captured is the only thing a
            // reader ever sees. Until the terminal is reaped there is none.
            ToolCallContent::Terminal(terminal) => {
                let terminal_id = terminal.terminal_id.0.as_ref();
                referenced.push(terminal_id);
                Some(
                    terminal_outputs
                        .iter()
                        .find(|record| record.terminal_id.as_str() == terminal_id)
                        .map(terminal_output_detail)
                        .or_else(|| raw_output.and_then(raw_output_terminal_detail))
                        .unwrap_or_else(|| format!("terminal {}", terminal.terminal_id)),
                )
            }
            _ => None,
        };
        if let Some(detail) = detail {
            details.push(sanitize_terminal_text(&detail));
        }
    }
    // Grok-style agents name the terminal on a mid-flight update and then
    // replace `content` wholesale without it, so the output hel captured has
    // nothing in the final call pointing at it. Show it rather than lose it.
    for record in terminal_outputs {
        if referenced.contains(&record.terminal_id.as_str()) {
            continue;
        }
        let output = sanitize_terminal_text(&record.output);
        if !output.is_empty() && details.iter().any(|detail| detail == &output) {
            // Kimi sends the captured stdout as ordinary tool content and in
            // its raw result. Keep the exit summary without printing those
            // same bytes a second time in Raw mode.
            details.push(terminal_exit_summary(record));
        } else {
            details.push(sanitize_terminal_text(&terminal_output_detail(record)));
        }
    }
    details
}

/// The output codex reports for a terminal it ran itself. Codex names its own
/// server-side terminal, which hel never opened and has no record for, and
/// puts the text in `rawOutput`; reading it here keeps such a call from
/// rendering as a bare terminal id.
fn raw_output_terminal_detail(raw_output: &serde_json::Value) -> Option<String> {
    let output = raw_output.get("formatted_output")?.as_str()?;
    let Some(exit_code) = raw_output
        .get("exit_code")
        .and_then(serde_json::Value::as_i64)
    else {
        return Some(output.to_owned());
    };
    let summary = format!("exited {exit_code}");
    if output.is_empty() {
        return Some(summary);
    }
    Some(format!("{output}\n{summary}"))
}

/// One terminal's output followed by how it ended.
pub fn terminal_output_detail(record: &TerminalOutputRecord) -> String {
    let summary = terminal_exit_summary(record);
    if record.output.is_empty() {
        return summary;
    }
    format!("{}\n{summary}", record.output)
}

/// How a terminal ended, in one line.
fn terminal_exit_summary(record: &TerminalOutputRecord) -> String {
    let mut summary = match (record.exit_code, &record.signal) {
        (_, Some(signal)) => format!("killed by {signal}"),
        (Some(code), None) => format!("exited {code}"),
        (None, None) => "released before exit".to_owned(),
    };
    if record.truncated {
        summary.push_str(" · output truncated");
    }
    summary
}

pub fn tool_diff_paths(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(diff.path.display().to_string()),
            _ => None,
        })
        .collect()
}

pub fn tool_location_details(locations: &[ToolCallLocation]) -> Vec<String> {
    locations
        .iter()
        .map(|location| match location.line {
            Some(line) => format!("{}:{line}", location.path.display()),
            None => location.path.display().to_string(),
        })
        .collect()
}

/// Append streamed agent or thought text to the transcript, merging it into
/// the entry it continues so a message arrives as one entry rather than one
/// per chunk.
pub(crate) fn push_streamed_entry(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    role: ChatRole,
    message_id: Option<String>,
    text: &str,
) {
    let text = sanitize_terminal_text(text);
    if let Some(last) = entries.last_mut()
        && last.role == role
        && (role == ChatRole::Thought || last.message_id == message_id)
    {
        last.touch(seq);
        if role == ChatRole::Thought
            && last.message_id != message_id
            && !last.text.is_empty()
            && !text.is_empty()
        {
            while last.text.ends_with('\n') {
                last.text.pop();
            }
            last.text.push('\n');
            last.text.push_str(text.trim_start_matches('\n'));
        } else {
            last.text.push_str(&text);
        }
        return;
    }
    let mut entry = ChatEntry::plain(seq, role, text).with_recorded_at(recorded_at_ms);
    entry.message_id = message_id;
    entries.push(entry);
}

/// Apply the transcript-visible part of one ACP session update. Returns the
/// update again when it changes the session surface rather than the
/// transcript, so the chat view handles those without decoding twice.
pub fn apply_session_update_to_entries(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    update: SessionUpdate,
) -> Option<SessionUpdate> {
    match update {
        SessionUpdate::AgentMessageChunk(chunk) => {
            let message_id = chunk.message_id.map(|id| id.to_string());
            if let Some(text) = content_block_text(&chunk.content) {
                push_streamed_entry(
                    entries,
                    seq,
                    recorded_at_ms,
                    ChatRole::Agent,
                    message_id,
                    &text,
                );
            }
        }
        SessionUpdate::AgentThoughtChunk(chunk) => {
            let message_id = chunk.message_id.map(|id| id.to_string());
            if let Some(text) = content_block_text(&chunk.content) {
                push_streamed_entry(
                    entries,
                    seq,
                    recorded_at_ms,
                    ChatRole::Thought,
                    message_id,
                    &text,
                );
            }
        }
        // PromptAccepted is the canonical local user-message event. ACP
        // user chunks would duplicate it during replay.
        SessionUpdate::UserMessageChunk(_) => {}
        SessionUpdate::ToolCall(call) => {
            let presentation = tool_call_presentation(&call);
            let mut entry = ChatEntry::tool(
                seq,
                call.title,
                Some(call.tool_call_id.to_string()),
                tool_status(&call.status),
            );
            entry.tool_summary = Some(presentation.summary.clone());
            entry.tool_presentation = Some(presentation);
            entry.tool_content = tool_content_details(&call.content, &[], call.raw_output.as_ref());
            entry.tool_diffstats = tool_diff_paths(&call.content);
            entry.tool_locations = tool_location_details(&call.locations);
            entries.push(entry);
        }
        SessionUpdate::ToolCallUpdate(update) => {
            let tool_call_id = update.tool_call_id.to_string();
            let entry = entries.iter_mut().rev().find(|entry| {
                entry.role == ChatRole::Tool
                    && entry.tool_call_id.as_deref() == Some(tool_call_id.as_str())
            })?;
            entry.touch(seq);
            let kind = update.fields.kind;
            let raw_input = update.fields.raw_input.clone();
            let raw_output = update.fields.raw_output.clone();
            if let Some(title) = update.fields.title {
                entry.text = sanitize_terminal_text(&title);
            }
            if let Some(status) = update.fields.status {
                entry.tool_status = Some(tool_status(&status));
            }
            if let Some(content) = update.fields.content {
                entry.tool_content =
                    tool_content_details(&content, &[], update.fields.raw_output.as_ref());
                entry.tool_diffstats = tool_diff_paths(&content);
            }
            if let Some(locations) = update.fields.locations {
                entry.tool_locations = tool_location_details(&locations);
            }
            let presentation = update_tool_call_presentation(
                entry.tool_presentation.as_ref(),
                &entry.text,
                kind,
                raw_input.as_ref(),
                raw_output.as_ref(),
            );
            entry.tool_summary = Some(presentation.summary.clone());
            entry.tool_presentation = Some(presentation);
        }
        SessionUpdate::Plan(plan) => {
            let lines = plan
                .entries
                .into_iter()
                .map(|entry| PlanLine {
                    text: sanitize_terminal_text(&entry.content),
                    status: plan_status(&entry.status),
                })
                .collect();
            let latest_user_seq = entries
                .iter()
                .rev()
                .find(|entry| entry.role == ChatRole::User)
                .map_or(0, |entry| entry.seq);
            if let Some(entry) = entries
                .iter_mut()
                .rev()
                .find(|entry| entry.role == ChatRole::Plan && entry.seq > latest_user_seq)
            {
                entry.touch(seq);
                entry.plan = lines;
            } else {
                entries.push(ChatEntry::plan(seq, lines));
            }
        }
        other => return Some(other),
    }
    None
}

/// Apply the transcript-visible part of one persisted runtime event. Returns
/// the event again when it only configures the session surface, which is the
/// chat view's business rather than the transcript's.
pub fn apply_runtime_event_to_entries(
    entries: &mut Vec<ChatEntry>,
    seq: u64,
    recorded_at_ms: Option<i64>,
    runtime: RuntimeEvent,
) -> Option<RuntimeEvent> {
    match runtime {
        RuntimeEvent::SessionUpdate { update } => {
            let parsed = match serde_json::from_value::<SessionUpdate>(update.clone()) {
                Ok(parsed) => parsed,
                Err(error) => {
                    tracing::debug!(%error, "ignoring invalid ACP session update");
                    return None;
                }
            };
            apply_session_update_to_entries(entries, seq, recorded_at_ms, parsed)
                .map(|_| RuntimeEvent::SessionUpdate { update })
        }
        RuntimeEvent::Warning { message } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                format!("warning: {message}"),
            ));
            None
        }
        RuntimeEvent::ConfigApplied { key, value, .. } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                format!("{key} set to {value}"),
            ));
            None
        }
        RuntimeEvent::SessionStarted { resumed: false, .. } => {
            entries.push(ChatEntry::plain(
                seq,
                ChatRole::System,
                "harness session started",
            ));
            None
        }
        RuntimeEvent::SessionStarted { resumed: true, .. } => None,
        other => Some(other),
    }
}

/// One transcript item flattened to the text a reader would see.
///
/// A caller that wants the structure reads the body itself; this is the plain
/// reading, built from the same flatteners every other surface uses so that a
/// tool call reads as the command it ran rather than as JSON.
pub fn transcript_item_text(item: &TranscriptItem) -> String {
    match &item.body {
        TranscriptBody::User { content } => materialized_content_text(content),
        TranscriptBody::Agent { chunks, .. } | TranscriptBody::Thought { chunks, .. } => {
            materialized_chunks_text(chunks)
        }
        TranscriptBody::Tool {
            call,
            terminal_outputs,
            presentation,
            ..
        } => {
            let Ok(call) = ToolCall::deserialize(call) else {
                return "[invalid tool call]".to_owned();
            };
            let mut text = materialized_tool_call_presentation(presentation.as_deref(), &call)
                .summary
                .clone();
            if text.trim().is_empty() {
                text = call.title.clone();
            }
            for record in terminal_outputs {
                text.push('\n');
                text.push_str(&terminal_output_detail(record));
            }
            sanitize_terminal_text(&text)
        }
        TranscriptBody::TerminalOutput { record } => {
            sanitize_terminal_text(&terminal_output_detail(record))
        }
        TranscriptBody::Plan { plan } => {
            let Ok(plan) = agent_client_protocol::schema::v1::Plan::deserialize(plan) else {
                return String::new();
            };
            plan.entries
                .iter()
                .map(|entry| {
                    let status = match plan_status(&entry.status) {
                        PlanStatus::Pending => "pending",
                        PlanStatus::Running => "running",
                        PlanStatus::Completed => "completed",
                    };
                    format!("[{status}] {}", sanitize_terminal_text(&entry.content))
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        TranscriptBody::PlanProposal { plan, .. } => plan.clone(),
        TranscriptBody::System { text } => text.clone(),
    }
}

pub(crate) fn compute_tool_diffstats(content: &[ToolCallContent]) -> Vec<String> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolCallContent::Diff(diff) => Some(format_diffstat(diff)),
            _ => None,
        })
        .collect()
}

pub fn materialized_tool_diffstats(item: &TranscriptItem) -> Option<Vec<String>> {
    let TranscriptBody::Tool { call, .. } = &item.body else {
        return None;
    };
    let call = match ToolCall::deserialize(call) {
        Ok(call) => call,
        Err(error) => {
            tracing::warn!(
                stable_id = %item.stable_id,
                %error,
                "could not decode a stored tool call while reading diff summary"
            );
            return None;
        }
    };
    if !matches!(
        tool_status(&call.status),
        ToolStatus::Completed | ToolStatus::Failed
    ) {
        return None;
    }
    let diffstats = compute_tool_diffstats(&call.content);
    (!diffstats.is_empty()).then_some(diffstats)
}

fn format_diffstat(diff: &agent_client_protocol::schema::v1::Diff) -> String {
    // A diff recorded since `diff` landed already carries its counts, so
    // this is a lookup. An older record still holds both file copies and is
    // diffed here on demand.
    let patch = mj_core::diff::patch_of(diff);
    format!(
        "{}  +{} −{}",
        diff.path.display(),
        patch.insertions,
        patch.deletions
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{ToolCall, ToolCallStatus};
    use serde_json::json;

    /// I1-18: Claude's question tool is titled with the question itself. Its
    /// row showed only "Which"; it must show the whole question.
    #[test]
    fn a_question_tool_is_summarized_by_its_whole_question() {
        let call = ToolCall::new("ask", "Which file name should I use for the new file?")
            .raw_input(json!({"questions": [{"question": "Which file name should I use for the new file?", "header": "File name"}]}));
        assert_eq!(
            tool_call_presentation(&call).summary,
            "Which file name should I use for the new file?"
        );
        let read = ToolCall::new("read", "Read src/lib.rs");
        assert_eq!(tool_call_presentation(&read).summary, "Read");
        let stale = ToolCallPresentation {
            summary: "Which".into(),
            source: call.title.clone(),
            source_kind: ToolSummarySourceKind::Title,
            tool_kind: ToolKind::Other,
            summary_version: 1,
        };
        assert_eq!(
            materialized_tool_call_presentation(Some(&stale), &call).summary,
            "Which file name should I use for the new file?",
            "stored rows written by the old rules are repaired"
        );
    }

    #[test]
    fn acp_new_file_diff_counts_each_inserted_line() {
        let diff = agent_client_protocol::schema::v1::Diff::new("/workspace/new.txt", "one\ntwo\n");

        assert_eq!(format_diffstat(&diff), "/workspace/new.txt  +2 \u{2212}0");
    }

    #[test]
    fn terminal_exit_summary_names_signal_release_and_truncation() {
        let record = |exit_code, signal: Option<&str>, truncated| TerminalOutputRecord {
            terminal_id: "term-1".into(),
            output: "out".into(),
            truncated,
            exit_code,
            signal: signal.map(str::to_owned),
        };

        assert_eq!(
            terminal_exit_summary(&record(Some(0), None, false)),
            "exited 0"
        );
        assert_eq!(
            terminal_exit_summary(&record(Some(1), None, true)),
            "exited 1 · output truncated"
        );
        assert_eq!(
            terminal_exit_summary(&record(None, Some("SIGKILL"), false)),
            "killed by SIGKILL"
        );
        assert_eq!(
            terminal_exit_summary(&record(None, None, false)),
            "released before exit"
        );

        // A terminal that produced nothing is still worth a line: the summary
        // is all a reader has to go on.
        let mut silent = record(None, Some("SIGTERM"), false);
        silent.output.clear();
        assert_eq!(terminal_output_detail(&silent), "killed by SIGTERM");
    }

    #[test]
    fn execute_shell_summary_keeps_commands_and_control_operators() {
        let call = ToolCall::new("call-1", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "cd dir && python x.py | cat | wc ; print ok"
            }));

        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.summary, "cd && python | cat | wc ; print");
        assert_eq!(presentation.source_kind, ToolSummarySourceKind::RawInput);
    }

    #[test]
    fn execute_sources_handle_shell_argv_and_ordinary_argv() {
        let shell = ToolCall::new("shell", "Terminal")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": ["bash", "-lc", "cd dir && python x.py | cat"]
            }));
        assert_eq!(tool_call_presentation(&shell).summary, "cd && python | cat");

        let argv = ToolCall::new("argv", "Execute")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": ["python", "-c", "print(1)"]}));
        assert_eq!(tool_call_presentation(&argv).summary, "python");
    }

    #[test]
    fn output_updates_reuse_input_command_summaries_but_changed_commands_do_not() {
        let call = ToolCall::new("shell", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "cargo test"}));
        let mut output = ToolCallUpdateFields::default();
        output.raw_output = Some(json!({"output": "x".repeat(128 * 1024)}));
        assert!(!tool_call_update_changes_presentation(&call, &output));
        let mut status = ToolCallUpdateFields::default();
        status.status = Some(ToolCallStatus::Completed);
        assert!(!tool_call_update_changes_presentation(&call, &status));
        let mut changed = ToolCallUpdateFields::default();
        changed.raw_input = Some(json!({"command": "cargo check"}));
        assert!(tool_call_update_changes_presentation(&call, &changed));
        let output_call = ToolCall::new("output", "Bash").kind(ToolKind::Execute);
        assert!(tool_call_update_changes_presentation(&output_call, &output));
    }

    fn execute_summary(command: serde_json::Value) -> String {
        let call = ToolCall::new("argv", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(command);
        tool_call_presentation(&call).summary
    }

    #[test]
    fn argv_summary_keeps_registered_command_verbs() {
        assert_eq!(
            execute_summary(json!({
                "command": ["git", "--no-pager", "status", "--short"]
            })),
            "git status"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["cargo", "+nightly", "test", "--package", "hel"]
            })),
            "cargo test"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["gh", "--hostname", "github.example", "pr", "list"]
            })),
            "gh pr list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["docker", "--context", "work", "compose", "up"]
            })),
            "docker compose up"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["podman", "machine", "list"]
            })),
            "podman machine list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["uv", "--project", "app", "pip", "install", "ruff"]
            })),
            "uv pip install"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["rustup", "toolchain", "list"]
            })),
            "rustup toolchain list"
        );
        assert_eq!(
            execute_summary(json!({
                "command": ["npm", "--prefix", "web", "run", "build"]
            })),
            "npm run"
        );
    }

    #[test]
    fn string_shell_and_argv_summaries_have_the_same_invocation_depth() {
        let string = ToolCall::new("string", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git --no-pager status --short"}));
        let argv = ToolCall::new("argv", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": ["git", "--no-pager", "status", "--short"]}));
        assert_eq!(
            tool_call_presentation(&string).summary,
            tool_call_presentation(&argv).summary
        );
    }

    #[test]
    fn unknown_leading_options_make_verb_position_ambiguous() {
        assert_eq!(
            execute_summary(json!({"command": ["git", "--mystery", "status"]})),
            "git"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--mystery", "test"]})),
            "cargo"
        );
    }

    #[test]
    fn non_whitelisted_commands_keep_only_the_executable() {
        assert_eq!(
            execute_summary(json!({"command": ["mytool", "build", "src"]})),
            "mytool"
        );
        assert_eq!(
            execute_summary(json!({"command": ["mytool", "./script.sh"]})),
            "mytool"
        );
        assert_eq!(
            execute_summary(json!({"command": ["python", "script.py"]})),
            "python"
        );
    }

    #[test]
    fn quoted_and_dynamic_shell_verbs_are_distinguished() {
        let quoted = ToolCall::new("quoted", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git \"status\""}));
        assert_eq!(tool_call_presentation(&quoted).summary, "git status");

        let dynamic = ToolCall::new("dynamic", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "git \"$verb\""}));
        assert_eq!(tool_call_presentation(&dynamic).summary, "git");

        let dynamic_name = ToolCall::new("dynamic-name", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "$command status"}));
        assert_eq!(tool_call_presentation(&dynamic_name).summary, "Bash");
    }

    #[test]
    fn wrappers_remain_direct_invocations() {
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("sudo", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "sudo -n git status"}))
            )
            .summary,
            "sudo"
        );
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("env", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "env FOO=bar git status"}))
            )
            .summary,
            "env"
        );
        assert_eq!(
            tool_call_presentation(
                &ToolCall::new("command", "Bash")
                    .kind(ToolKind::Execute)
                    .raw_input(json!({"command": "command git status"}))
            )
            .summary,
            "command"
        );
    }

    #[test]
    fn second_verbs_require_a_registered_namespace() {
        assert_eq!(
            execute_summary(json!({"command": ["gh", "api", "graphql"]})),
            "gh api"
        );
        assert_eq!(
            execute_summary(json!({"command": ["docker", "run", "ubuntu"]})),
            "docker run"
        );
        assert_eq!(
            execute_summary(json!({"command": ["git", "future-verb"]})),
            "git future-verb"
        );
    }

    #[test]
    fn known_attached_and_short_global_options_are_skipped() {
        assert_eq!(
            execute_summary(json!({"command": ["/usr/bin/git", "-Crepo", "status"]})),
            "/usr/bin/git status"
        );
        assert_eq!(
            execute_summary(json!({"command": ["git", "-c", "core.pager=cat", "status"]})),
            "git status"
        );
        assert_eq!(
            execute_summary(json!({"command": ["gh", "-Rorg/repo", "pr", "list"]})),
            "gh pr list"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--color=always", "test"]})),
            "cargo test"
        );
        assert_eq!(
            execute_summary(json!({"command": ["cargo", "--config", "build.jobs=2", "test"]})),
            "cargo test"
        );
    }

    #[test]
    fn shell_summary_skips_assignments_arguments_and_nested_substitutions() {
        let call = ToolCall::new("call", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "FOO=bar env -i bash -c \"echo $(printf hi)\""
            }));
        assert_eq!(tool_call_presentation(&call).summary, "env");

        let subshell = ToolCall::new("subshell", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "(cd dir && python x.py) | cat"
            }));
        assert_eq!(
            tool_call_presentation(&subshell).summary,
            "(cd && python) | cat"
        );
    }

    #[test]
    fn shell_summary_keeps_list_pipeline_and_background_operators() {
        let call = ToolCall::new("operators", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "'printf' '%s' hi >out |& sed s/hi/bye/ || echo failed & wait; cat <in"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "printf |& sed || echo & wait ; cat"
        );
    }

    #[test]
    fn shell_summary_removes_structural_loop_and_group_separators() {
        let call = ToolCall::new("compound", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "for file in a b; do rm \"$file\"; done; mkdir -p out; nice -n 10 python3 script.py"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "rm ; mkdir ; nice python3"
        );

        let conditional = ToolCall::new("conditional", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "if test -f foo; then rm foo; fi; { mkdir bar; echo done; }"
            }));
        assert_eq!(
            tool_call_presentation(&conditional).summary,
            "test ; rm ; mkdir ; echo"
        );

        let case_statement = ToolCall::new("case", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "echo start; case x in a|b) echo branch;; esac; echo done"
            }));
        assert_eq!(
            tool_call_presentation(&case_statement).summary,
            "echo ; echo ; echo"
        );
    }

    #[test]
    fn shell_summary_handles_a_loop_with_a_leading_pipeline_and_nice() {
        let call = ToolCall::new("live-loop-shape", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "cd /tmp && for spec in a b; do set -- $spec; rm \"$spec\"; mkdir -p \"$spec\"; nice -n 10 ./bin/bifrost \"$spec\"; echo \"$spec\"; done; python3 script.py"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "cd && set ; rm ; mkdir ; nice ./bin/bifrost ; echo ; python3"
        );
    }

    #[test]
    fn shell_summary_inserts_a_separator_after_a_heredoc() {
        let call = ToolCall::new("heredoc", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({
                "command": "python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head"
            }));

        assert_eq!(
            tool_call_presentation(&call).summary,
            "python3 ; grep | head"
        );
    }

    #[test]
    fn materialized_summary_from_an_older_parser_is_repaired() {
        let source = "python3 <<'PYEOF'\nprint(\"x\")\nPYEOF\ngrep -n x file | head";
        let call = ToolCall::new("heredoc", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({ "command": source }));
        let stale = ToolCallPresentation {
            summary: "python3 grep | head".into(),
            source: source.into(),
            source_kind: ToolSummarySourceKind::RawInput,
            tool_kind: ToolKind::Execute,
            summary_version: 0,
        };

        let repaired = materialized_tool_call_presentation(Some(&stale), &call);
        assert_eq!(repaired.summary, "python3 ; grep | head");
        assert_eq!(repaired.summary_version, TOOL_SUMMARY_VERSION);

        let mut current = repaired;
        current.summary = "stored current summary".into();
        assert_eq!(
            materialized_tool_call_presentation(Some(&current), &call).summary,
            "stored current summary"
        );
    }

    #[test]
    fn nice_summary_skips_its_known_adjustment_options() {
        for command in [
            vec!["nice", "-n", "10", "python3", "script.py"],
            vec!["nice", "--adjustment", "10", "python3", "script.py"],
            vec!["nice", "--adjustment=10", "python3", "script.py"],
            vec!["nice", "-10", "python3", "script.py"],
            vec!["nice", "-n10", "python3", "script.py"],
            vec!["nice", "--", "python3", "script.py"],
        ] {
            assert_eq!(execute_summary(json!({"command": command})), "nice python3");
        }

        let string = ToolCall::new("nice-string", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "nice -n 10 python3 script.py"}));
        assert_eq!(tool_call_presentation(&string).summary, "nice python3");
    }

    #[test]
    fn execute_summary_bounds_the_retained_source_before_parsing() {
        let command = format!("echo {}", "argument".repeat(TOOL_SUMMARY_SOURCE_BYTES));
        let call = ToolCall::new("bounded", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({ "command": command }));

        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.source.len(), TOOL_SUMMARY_SOURCE_BYTES);
        assert_eq!(presentation.summary, "echo");
    }

    #[test]
    fn non_execute_titles_use_the_first_meaningful_token() {
        let call = ToolCall::new("read", "Read src/lib.rs").kind(ToolKind::Read);
        let presentation = tool_call_presentation(&call);
        assert_eq!(presentation.summary, "Read");
        assert_eq!(presentation.source_kind, ToolSummarySourceKind::Title);
    }

    #[test]
    fn title_wrappers_and_malformed_shell_fall_back_safely() {
        let wrapped = ToolCall::new("wrapped", "Running: ls -la").kind(ToolKind::Execute);
        assert_eq!(tool_call_presentation(&wrapped).summary, "ls");

        let malformed = ToolCall::new("bad", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "cd && ("}));
        assert_eq!(tool_call_presentation(&malformed).summary, "Bash");
    }

    #[test]
    fn explicit_empty_raw_input_drops_a_stale_raw_summary() {
        let initial = ToolCall::new("call", "Bash")
            .kind(ToolKind::Execute)
            .raw_input(json!({"command": "python script.py"}));
        let previous = tool_call_presentation(&initial);
        let empty_input = json!({"command": null});
        let updated = update_tool_call_presentation(
            Some(&previous),
            "Running: ls -la",
            None,
            Some(&empty_input),
            None,
        );
        assert_eq!(updated.summary, "ls");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::Title);

        let output = json!({"command": "cat result.txt"});
        let updated = update_tool_call_presentation(
            Some(&previous),
            "Running: ls -la",
            None,
            Some(&empty_input),
            Some(&output),
        );
        assert_eq!(updated.summary, "cat");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::RawOutput);

        let output_initial = ToolCall::new("output", "Bash")
            .kind(ToolKind::Execute)
            .raw_output(json!({"command": "python result.py"}));
        let output_previous = tool_call_presentation(&output_initial);
        let empty_output = json!({"command": null});
        let updated = update_tool_call_presentation(
            Some(&output_previous),
            "Running: ls -la",
            None,
            None,
            Some(&empty_output),
        );
        assert_eq!(updated.summary, "ls");
        assert_eq!(updated.source_kind, ToolSummarySourceKind::Title);
    }
}
