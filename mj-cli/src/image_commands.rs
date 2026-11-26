use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use mj_client::session_image::{ImageRequest, ImageStatus};

use crate::api_client::ApiClient;

#[derive(Debug, Args)]
pub(crate) struct ImageArgs {
    #[command(subcommand)]
    command: ImageCommand,
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// Print a Containerfile baking every pinned harness into a base image.
    Base(BaseArgs),
    /// Build, push, and assign an image from the repository's mise recipe.
    Build(BuildArgs),
    /// Compare published project images with the repository's current recipe.
    Status(StatusArgs),
}

#[derive(Debug, Args)]
struct BaseArgs {
    #[arg(long)]
    base: String,
    #[arg(long)]
    platform: String,
    #[arg(long, default_value = "")]
    user: String,
    #[arg(long, default_value = "/")]
    workdir: String,
}

#[derive(Debug, Args)]
struct BuildArgs {
    /// Directory inside the repository; defaults to the current directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Bundle to assign; defaults to the session's bundle or this local repository.
    #[arg(long)]
    bundle: Option<String>,
    /// Container target whose configured image supplies the base.
    #[arg(long)]
    target: Option<String>,
    /// Registry image name; defaults to the bundle id.
    #[arg(long)]
    name: Option<String>,
    /// Print the deterministic Containerfile without building or publishing.
    #[arg(long)]
    print: bool,
}

#[derive(Debug, Args)]
struct StatusArgs {
    /// Directory inside the repository; defaults to the current directory.
    #[arg(long)]
    path: Option<PathBuf>,
    /// Bundle to inspect; defaults to the session's bundle or this local repository.
    #[arg(long)]
    bundle: Option<String>,
    /// Target whose base image determines the expected recipe hash.
    #[arg(long)]
    target: Option<String>,
}

fn collect(path: Option<PathBuf>) -> Result<(PathBuf, BTreeMap<String, String>)> {
    let path = path
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()
        .context("resolve recipe path")?;
    let root = path
        .ancestors()
        .find(|ancestor| ancestor.join(".git").exists())
        .context("--path must be inside a repository")?;
    let mut files = BTreeMap::new();
    for name in mj_harness_install::session_image::RECIPE_FILES {
        match std::fs::read_to_string(root.join(name)) {
            Ok(text) => {
                files.insert((*name).to_owned(), text);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| format!("read recipe {name}")),
        }
    }
    Ok((root.to_path_buf(), files))
}

pub(crate) async fn run(args: ImageArgs) -> Result<()> {
    let (path, bundle, target, name, print, status) = match args.command {
        ImageCommand::Base(args) => {
            let base = mj_harness_install::session_image::ImageBase {
                reference: args.base,
                user: args.user,
                workdir: args.workdir,
                ..Default::default()
            };
            print!(
                "{}",
                mj_harness_install::session_image::session_base_containerfile(
                    &base,
                    &args.platform
                )?
            );
            return Ok(());
        }
        ImageCommand::Build(args) => (
            args.path,
            args.bundle,
            args.target,
            args.name,
            args.print,
            false,
        ),
        ImageCommand::Status(args) => (args.path, args.bundle, args.target, None, false, true),
    };
    let (path, files) = mj_core::runtime::spawn_blocking(move || collect(path))
        .await
        .context("join recipe collection")??;
    let request = ImageRequest {
        path,
        files,
        bundle,
        target,
        name,
        session_id: std::env::var("MJ_SESSION_ID").ok(),
        print,
    };
    let mut client = ApiClient::connect().await?;
    let mut started = client.image_start(&request, status).await?;
    let mut offset = 0;
    let mut resubmitted = false;
    loop {
        let following = tokio::select! {
            answer = client.image_follow(&started.operation_id, offset) => answer,
            signal = tokio::signal::ctrl_c() => {
                signal.context("wait for Ctrl-C")?;
                client.image_cancel(&started.operation_id).await?;
                // Follow until the daemon confirms subprocess cleanup completed.
                loop {
                    match client.image_follow(&started.operation_id, offset).await? {
                        Some(follow) => {
                            offset = follow.next_offset;
                            if let Some(result) = follow.result {
                                return match result {
                                    Ok(result) => {
                                        eprintln!("The image operation completed before cancellation.");
                                        print_result(result, status)
                                    }
                                    Err(_) => Err(anyhow::anyhow!("image build cancelled")),
                                };
                            }
                        }
                        None => bail!("image operation ended with the previous daemon"),
                    }
                }
            }
        };
        match following {
            Ok(Some(follow)) => {
                if !print {
                    std::io::stderr().write_all(follow.log.as_bytes())?;
                }
                offset = follow.next_offset;
                if let Some(result) = follow.result {
                    match result {
                        Ok(result) => {
                            return print_result(result, status);
                        }
                        Err(error) => {
                            if !resubmitted && error.contains("cancelled") {
                                let next = ApiClient::connect().await?;
                                if next.daemon_instance() != client.daemon_instance() {
                                    eprintln!(
                                        "The daemon was replaced; resubmitting this reproducible image operation once."
                                    );
                                    client = next;
                                    started = client.image_start(&request, status).await?;
                                    offset = 0;
                                    resubmitted = true;
                                    continue;
                                }
                            }
                            bail!("{error}");
                        }
                    }
                }
            }
            failed => {
                let mut next = ApiClient::connect().await?;
                if failed
                    .as_ref()
                    .is_err_and(crate::api_client::is_daemon_handoff)
                {
                    while next.daemon_instance() == client.daemon_instance() {
                        next = ApiClient::connect_after_handoff().await?;
                    }
                }
                if !resubmitted && next.daemon_instance() != client.daemon_instance() {
                    eprintln!(
                        "The daemon was replaced; resubmitting this reproducible image operation once."
                    );
                    client = next;
                    started = client.image_start(&request, status).await?;
                    offset = 0;
                    resubmitted = true;
                } else {
                    match failed {
                        Err(error) => return Err(error),
                        _ => bail!(
                            "unknown image operation on the current daemon; rerun mj image build"
                        ),
                    }
                }
            }
        }
    }
}

fn print_result(result: mj_client::session_image::ImageResult, status: bool) -> Result<()> {
    if let Some(text) = result.containerfile {
        print!("{text}");
    }
    if let Some(image) = result.image {
        println!("{image}");
    }
    if status {
        print!("{}", render_status(&result.status));
    }
    Ok(())
}

fn render_status(status: &[ImageStatus]) -> String {
    if status.is_empty() {
        return "No project images are recorded. Run mj image build to publish one.\n".into();
    }
    status
        .iter()
        .map(|entry| {
            format!(
                "{}  {}  {}\n",
                entry.platform,
                entry.image,
                match entry.current {
                    Some(true) => "current".to_owned(),
                    Some(false) => "out of date".to_owned(),
                    None if entry.no_builder =>
                        format!("unknown (no builder for {})", entry.platform),
                    None => "unknown (use --target ID)".to_owned(),
                }
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_build_print_golden() {
        let base = mj_harness_install::session_image::ImageBase {
            reference: format!(
                "ghcr.io/brokkai/mjolnir/session-base@sha256:{}",
                "a".repeat(64)
            ),
            user: "hel".into(),
            workdir: "/workspace".into(),
            ..Default::default()
        };
        let mise = mj_harness_install::session_image::MiseArtifact { version: "2026.11.9".into(), url: "https://github.com/jdx/mise/releases/download/v2026.11.9/mise-v2026.11.9-linux-x64-musl".into(), sha256: "e55de2c37a08a9dfdb3ca95490f90f914b5ef362b62761f1bb2110e5d57210f7".into() };
        let files = BTreeMap::from([
            (".nvmrc".into(), "24\n".into()),
            ("mise.toml".into(), "[tools]\npython = \"3.15\"\n".into()),
        ]);
        let text = mj_harness_install::session_image::containerfile(
            &base,
            "linux/amd64",
            &mise,
            &files,
            &[],
        )
        .unwrap();
        mj_core::golden::assert_golden(env!("CARGO_MANIFEST_DIR"), "image-build-print", &text);
    }

    #[test]
    fn image_status_golden() {
        let status = [
            ImageStatus {
                platform: "linux/amd64".into(),
                image: "localhost:5000/projects/app:0123456789abcdef".into(),
                current: Some(true),
                no_builder: false,
            },
            ImageStatus {
                platform: "linux/arm64".into(),
                image: "localhost:5000/projects/app:fedcba9876543210".into(),
                current: Some(false),
                no_builder: false,
            },
            ImageStatus {
                platform: "linux/riscv64".into(),
                image: "localhost:5000/projects/app:0011223344556677".into(),
                current: None,
                no_builder: false,
            },
        ];
        mj_core::golden::assert_golden(
            env!("CARGO_MANIFEST_DIR"),
            "image-status",
            &render_status(&status),
        );
    }
}
