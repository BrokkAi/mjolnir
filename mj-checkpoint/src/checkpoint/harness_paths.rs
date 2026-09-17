use super::*;
use mj_core::hex::lower_hex;

pub(super) fn rewrite_kimi_workspace_registry(data: &[u8], target_cwd: &Path) -> Result<Vec<u8>> {
    let mut registry: Value =
        serde_json::from_slice(data).context("parse Kimi workspace registry")?;
    let workspaces = registry
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .context("Kimi workspace registry has no workspaces object")?;
    ensure!(
        workspaces.len() == 1,
        "Kimi workspace registry must contain one imported workspace"
    );
    let (_, mut workspace) = std::mem::take(workspaces)
        .into_iter()
        .next()
        .expect("one workspace was checked");
    let workspace = workspace
        .as_object_mut()
        .context("Kimi workspace registry entry is not an object")?;
    workspace.insert(
        "root".into(),
        Value::String(target_cwd.to_string_lossy().into_owned()),
    );
    if let Some(name) = target_cwd.file_name().and_then(|name| name.to_str()) {
        workspace.insert("name".into(), Value::String(name.to_owned()));
    }
    workspaces.insert(kimi_workspace_key(target_cwd), workspace.clone().into());
    Ok(serde_json::to_vec(&registry)?)
}

pub(super) fn rewrite_kimi_session_index(
    data: &[u8],
    target_cwd: &Path,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let mut rewritten = Vec::new();
    let target_workspace = kimi_workspace_key(target_cwd);
    for (line_number, line) in std::str::from_utf8(data)
        .context("decode Kimi session index")?
        .lines()
        .enumerate()
    {
        if line.trim().is_empty() {
            continue;
        }
        let mut entry: Value = serde_json::from_str(line)
            .with_context(|| format!("parse Kimi session index line {}", line_number + 1))?;
        let session_id = entry
            .get("sessionId")
            .and_then(Value::as_str)
            .context("Kimi session index entry lacks sessionId")?
            .to_owned();
        let entry = entry
            .as_object_mut()
            .context("Kimi session index entry is not an object")?;
        entry.insert(
            "workDir".into(),
            Value::String(target_cwd.to_string_lossy().into_owned()),
        );
        entry.insert(
            "sessionDir".into(),
            Value::String(
                harness_home
                    .join("sessions")
                    .join(&target_workspace)
                    .join(session_id)
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
        serde_json::to_writer(&mut rewritten, &entry)?;
        rewritten.push(b'\n');
    }
    ensure!(
        !rewritten.is_empty(),
        "Kimi session index has no imported sessions"
    );
    Ok(rewritten)
}

pub(super) fn is_kimi_session_state(relative_path: &Path) -> bool {
    let mut components = relative_path.components();
    matches!(components.next(), Some(Component::Normal(component)) if component == "sessions")
        && matches!(components.next(), Some(Component::Normal(_)))
        && matches!(components.next(), Some(Component::Normal(component)) if component.to_string_lossy().starts_with("session_"))
        && matches!(components.next(), Some(Component::Normal(component)) if component == "state.json")
        && components.next().is_none()
}

pub(super) fn is_grok_session_summary(relative_path: &Path) -> bool {
    grok_session_components(relative_path)
        .is_some_and(|components| components.file == "summary.json")
}

/// Grok Build records the session's working directory and home in
/// `summary.json`; both must follow the restored session to its new workspace.
pub(super) fn rewrite_grok_session_summary(
    data: &[u8],
    target_cwd: &Path,
    harness_home: &Path,
) -> Result<Vec<u8>> {
    let mut summary: Value =
        serde_json::from_slice(data).context("parse Grok Build session summary")?;
    let object = summary
        .as_object_mut()
        .context("Grok Build session summary is not a JSON object")?;
    if object.contains_key("grok_home") {
        object.insert(
            "grok_home".into(),
            Value::String(harness_home.to_string_lossy().into_owned()),
        );
    }
    if let Some(info) = object.get_mut("info").and_then(Value::as_object_mut)
        && info.contains_key("cwd")
    {
        info.insert(
            "cwd".into(),
            Value::String(target_cwd.to_string_lossy().into_owned()),
        );
    }
    Ok(serde_json::to_vec(&summary)?)
}

pub(super) struct GrokSessionPath<'a> {
    session: &'a str,
    file: &'a str,
}

/// Split `sessions/<cwd-key>/<session-uuid>/<file>` into the parts Hel needs.
/// Anything with a different shape is not a Grok Build session artifact.
pub(super) fn grok_session_components(relative: &Path) -> Option<GrokSessionPath<'_>> {
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(component) => component.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let [root, _cwd_key, session, file] = components.as_slice() else {
        return None;
    };
    (*root == "sessions").then_some(GrokSessionPath { session, file })
}

/// Runtime state that must not travel in a checkpoint: advisory lock files and
/// the sessions-wide search index.
pub(super) fn grok_session_artifact(relative: &Path, session_id: &str) -> bool {
    grok_session_components(relative).is_some_and(|components| {
        components.session == session_id
            && !components.file.ends_with(".lock")
            && !components.file.starts_with("session_search.sqlite")
    })
}

/// Grok Build's on-disk cwd-key algorithm, replicated from grok-build
/// `xai-grok-config::paths::encode_cwd_dirname`: URL-encode the working
/// directory, or fall back to `{slug}-{blake3-hex-16}` when that would exceed
/// one filesystem name.
pub(super) fn grok_cwd_key(cwd: &Path) -> String {
    /// macOS APFS, Linux ext4, and NTFS all cap a name at 255 bytes.
    const MAX_DIRNAME_BYTES: usize = 255;
    const MAX_SLUG_CHARS: usize = 40;

    let cwd = cwd.to_string_lossy();
    let encoded = url_encode(&cwd);
    if encoded.len() <= MAX_DIRNAME_BYTES {
        return encoded;
    }
    let digest = blake3::hash(cwd.as_bytes()).to_hex();
    let leaf = Path::new(cwd.as_ref())
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let slug = grok_slug(leaf, MAX_SLUG_CHARS);
    let slug = if slug.is_empty() { "workspace" } else { &slug };
    format!("{slug}-{}", &digest[..16])
}

/// Percent-encode every byte outside the RFC 3986 unreserved set, matching the
/// `urlencoding` crate Grok Build uses.
pub(super) fn url_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// Grok Build's `slugify`: lowercase, non-alphanumerics collapse to a single
/// dash, trim dashes, truncate to `max_chars`.
pub(super) fn grok_slug(input: &str, max_chars: usize) -> String {
    let mut slug = String::with_capacity(input.len());
    let mut previous_dash = false;
    for character in input.to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            previous_dash = false;
        } else if !previous_dash {
            slug.push('-');
            previous_dash = true;
        }
    }
    slug.trim_matches('-').chars().take(max_chars).collect()
}

/// Claude Code's on-disk project-key algorithm, captured from local rollouts:
/// every non-ASCII-alphanumeric cwd character becomes a hyphen.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

/// Kimi Code keys session directories by the final cwd component and the
/// first 12 hexadecimal digits of SHA-256(cwd), captured from real rollouts.
pub(super) fn kimi_workspace_key(cwd: &Path) -> String {
    let basename = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace");
    let digest = lower_hex(Sha256::digest(cwd.to_string_lossy().as_bytes()));
    format!("wd_{basename}_{}", &digest[..12])
}
