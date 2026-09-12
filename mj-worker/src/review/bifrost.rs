//! Target-side semantic review execution.
use mj_core::review::bifrost::*;
use mj_core::review::{CHANGED_FUNCTIONS_LIMIT, bound_review_section};
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
    let binary = bifrost_binary();
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
    let mut command = tokio::process::Command::new(&binary);
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
}
