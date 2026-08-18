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
    let status = tokio::process::Command::new(&invocation.command)
        .args(&invocation.args)
        .envs(&invocation.env)
        .status()
        .await
        .with_context(|| format!("run {} login", vendor.label()))?;
    let success = status.success();
    let credentials_available = success && detect(vendor).available();
    login_outcome_from_status(vendor, success, &status.to_string(), credentials_available)
}
