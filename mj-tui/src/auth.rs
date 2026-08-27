//! Interactive authentication frontend.

use anyhow::{Context, Result};

pub use mj_core::auth::*;

pub async fn run_login(vendor: AuthVendor) -> Result<LoginOutcome> {
    let args = match vendor {
        AuthVendor::OpenAi => {
            let options = [
                crate::menu::MenuOption {
                    label: "Browser",
                    hint: "codex login".to_string(),
                    shortcuts: &['b'],
                },
                crate::menu::MenuOption {
                    label: "Device code",
                    hint: "codex login --device-auth".to_string(),
                    shortcuts: &['d'],
                },
            ];
            let Some(selected) = crate::menu::select_inline_cancelable(
                "OpenAI / ChatGPT sign-in",
                "Enter confirms · Esc cancels",
                &options,
                0,
            )?
            else {
                return Ok(LoginOutcome::Cancelled(
                    "OpenAI / ChatGPT sign-in cancelled".to_string(),
                ));
            };
            login_args_for_selection(selected)
        }
        AuthVendor::Anthropic => {
            let options = [
                crate::menu::MenuOption {
                    label: "Claude subscription",
                    hint: "Claude Pro, Max, Team, or Enterprise".to_string(),
                    shortcuts: &['s'],
                },
                crate::menu::MenuOption {
                    label: "Anthropic Console",
                    hint: "API usage billing".to_string(),
                    shortcuts: &['c'],
                },
            ];
            let Some(selected) = crate::menu::select_inline_cancelable(
                "Anthropic / Claude sign-in",
                "Enter confirms · Esc cancels",
                &options,
                0,
            )?
            else {
                return Ok(LoginOutcome::Cancelled(
                    "Anthropic / Claude sign-in cancelled".to_string(),
                ));
            };
            anthropic_login_args(selected == 1)
        }
    };
    println!(
        "Signing in to {}. Mjolnir will return when it finishes.",
        vendor.label()
    );
    if let Some(hint) = login_terminal_hint(vendor) {
        println!("{hint}");
    }
    println!();
    let mut invocation = bundled_invocation(vendor).await?;
    append_login_args(&mut invocation, args);
    let _interrupt_guard = crate::termination::suppress_interrupts();
    let mut repaired = false;
    let status = loop {
        let (status, stderr_text) = run_login_command(vendor, &invocation).await?;
        // One-shot recovery: a failed launch whose stderr implicates the npx
        // cache means an interrupted install poisoned the entry; remove it
        // and retry once so the reinstall happens without user intervention.
        if !status.success()
            && !repaired
            && mj_core::npx_repair::repair_after_failure(
                &invocation.args,
                &invocation.env,
                &stderr_text,
            )
            .await
            .is_some()
        {
            repaired = true;
            println!("Removed a corrupted npx cache entry; retrying sign-in.");
            continue;
        }
        break status;
    };
    let success = status.success();
    let credentials_available = success && detect(vendor).available();
    login_outcome_from_status(vendor, success, &status.to_string(), credentials_available)
}

/// Run the login CLI with stdin/stdout inherited (the flows are interactive)
/// while teeing stderr through, so a failure's output is available for the
/// npx cache check without changing what the user sees.
async fn run_login_command(
    vendor: AuthVendor,
    invocation: &LoginInvocation,
) -> Result<(std::process::ExitStatus, String)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const STDERR_TAIL_LIMIT: usize = 64 * 1024;
    let mut child = tokio::process::Command::new(&invocation.command)
        .args(&invocation.args)
        .envs(&invocation.env)
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("run {} login", vendor.label()))?;
    let mut stderr = child.stderr.take().expect("piped stderr");
    let tee = tokio::spawn(async move {
        let mut tail: Vec<u8> = Vec::new();
        let mut buffer = [0u8; 4096];
        let mut out = tokio::io::stderr();
        loop {
            match stderr.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let _ = out.write_all(&buffer[..read]).await;
                    let _ = out.flush().await;
                    tail.extend_from_slice(&buffer[..read]);
                    if tail.len() > STDERR_TAIL_LIMIT {
                        let excess = tail.len() - STDERR_TAIL_LIMIT;
                        tail.drain(..excess);
                    }
                }
            }
        }
        tail
    });
    let status = child
        .wait()
        .await
        .with_context(|| format!("wait for {} login", vendor.label()))?;
    let tail = tee.await.unwrap_or_default();
    Ok((status, String::from_utf8_lossy(&tail).into_owned()))
}
