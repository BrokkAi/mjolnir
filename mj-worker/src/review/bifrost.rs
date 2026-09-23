//! Target-side semantic review execution.
use mj_review::bifrost::*;
use mj_review::{CHANGED_FUNCTIONS_LIMIT, bound_review_section};
use std::process::Stdio;
/// Runs `analyze_diff` over each repository and renders one packet.
///
/// Every repository must succeed. A partial packet would tell the supervisor
/// that a repository changed nothing when in truth Bifrost could not read it,
/// which is the fabricated-evidence failure the whole design refuses.
pub async fn changed_functions_packet(requests: &[AnalyzeRequest]) -> Result<String, String> {
    let mut sections = Vec::new();
    for request in requests {
        let analysis = analyze_diff(request).await?;
        sections.push(format!(
            "Repository: {}\n{}",
            request.repository.display(),
            format_changed_functions(&analysis)
        ));
    }
    Ok(bound_review_section(
        &sections.join("\n\n"),
        CHANGED_FUNCTIONS_LIMIT,
        "changed functions",
    ))
}

/// Runs Bifrost's one-shot `analyze_diff` for one repository.
pub async fn analyze_diff(request: &AnalyzeRequest) -> Result<AnalyzeDiffResult, String> {
    analyze_diff_with(&bifrost_binary(), request).await
}

async fn analyze_diff_with(
    binary: &std::path::Path,
    request: &AnalyzeRequest,
) -> Result<AnalyzeDiffResult, String> {
    tracing::info!(
        event = "review_analyze_diff_started",
        bifrost = %binary.display(),
        root = %request.repository.display(),
        base_tree = %request.base_tree,
        target_tree = %request.target_tree,
        "running bifrost analyze_diff for the captured turn trees"
    );
    let args = serde_json::json!({
        "base": request.base_tree,
        "target": request.target_tree,
    })
    .to_string();
    let mut command = tokio::process::Command::new(binary);
    command
        .current_dir(&request.repository)
        .kill_on_drop(true)
        // The capture ran against a scratch index; a review must never inherit
        // it, or Bifrost would read a half-written index as the repository.
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .arg("--root")
        .arg(&request.repository)
        .args(["--tool", "analyze_diff", "--args"])
        .arg(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // `output()` drains stdout and stderr concurrently with the wait, so a
    // child that fills a pipe buffer cannot deadlock this call. stdin is
    // closed, so there is nothing to feed it.
    let output = match tokio::time::timeout(ANALYZE_DIFF_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the review needs the `{}` binary, which this session's container image does not have; rebuild the image so it includes Bifrost",
                binary.display()
            ));
        }
        Ok(Err(error)) => return Err(format!("could not run bifrost: {error}")),
        Err(_) => {
            return Err(format!(
                "bifrost analysis of {} exceeded its {}s budget",
                request.repository.display(),
                ANALYZE_DIFF_TIMEOUT.as_secs()
            ));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("Unknown tool") {
            return Err(incompatible_bifrost(binary).await);
        }
        return Err(format!(
            "bifrost exited with {}: {}",
            output.status,
            stderr.trim()
        ));
    }
    let envelope: AnalyzeDiffEnvelope = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("invalid analyze_diff JSON: {error}"))?;
    Ok(envelope.structured_content)
}

/// Explains a Bifrost that does not offer `analyze_diff`: an older release
/// found on PATH instead of the one the review was built against.
async fn incompatible_bifrost(binary: &std::path::Path) -> String {
    let version = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .arg("--version")
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|version| !version.is_empty())
    .unwrap_or_else(|| "an unknown version".to_owned());
    format!(
        "the `{}` binary is {version}, which has no analyze_diff tool; the review needs Bifrost {REQUIRED_BIFROST_VERSION} or later (`cargo install brokk-bifrost@{REQUIRED_BIFROST_VERSION} --locked --bin bifrost`, or set {BIFROST_BIN_ENV} to a newer binary)",
        binary.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_missing_bifrost_binary_fails_the_review_with_the_fix() {
        // Safety: the env var is process-global; this test names a unique
        // path and does not race another test that reads it, because no other
        // test in this module spawns Bifrost.
        unsafe {
            std::env::set_var(BIFROST_BIN_ENV, "/nonexistent/hel-review-bifrost");
        }
        let error = analyze_diff(&AnalyzeRequest {
            repository: std::env::temp_dir(),
            base_tree: "base".to_string(),
            target_tree: "target".to_string(),
        })
        .await
        .expect_err("a missing binary must fail the review");
        unsafe {
            std::env::remove_var(BIFROST_BIN_ENV);
        }
        assert!(error.contains("rebuild the image"), "{error}");
    }

    /// I1-15: an old Bifrost on PATH fails every analysis with "Unknown
    /// tool: analyze_diff". The review says which binary and version it
    /// found and what it needs, rather than passing the raw error on.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_old_bifrost_is_reported_with_its_version_and_the_fix() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("bifrost");
        std::fs::write(
            &fake,
            "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'bifrost 0.7.5'; exit 0; fi\necho 'Unknown tool: analyze_diff' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = analyze_diff_with(
            &fake,
            &AnalyzeRequest {
                repository: temp.path().to_path_buf(),
                base_tree: "base".to_string(),
                target_tree: "target".to_string(),
            },
        )
        .await
        .expect_err("an old bifrost must fail the analysis");
        assert!(error.contains("bifrost 0.7.5"), "{error}");
        assert!(error.contains("has no analyze_diff tool"), "{error}");
        assert!(error.contains(REQUIRED_BIFROST_VERSION), "{error}");
    }
}
