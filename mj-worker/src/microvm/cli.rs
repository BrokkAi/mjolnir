//! The Podman-compatible command line of the engine.
//!
//! Only the verbs, flags and output formats that Mjolnir's Podman code uses
//! exist. Anything else is an error that names it: nothing is ignored
//! silently. Engine failures exit 125, as Podman's do.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use super::image::{self, PullPolicy};
use super::protocol::ExecRequest;
use super::state::{ImageConfig, Layout, VolumeMetadata, validate_label, validate_name};
use super::vm::{self, RunSpec, VolumeArg};
use super::{net, sys, template, volume};

/// The exit status of an engine failure, as Podman uses.
const ENGINE_FAILURE: i32 = 125;

#[derive(Debug, Parser)]
#[command(
    name = "mj-worker microvm",
    about = "Run OCI images as Nitro Sandbox microVMs with a Podman-compatible command line",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report the engine and host facts.
    Info(InfoArgs),
    /// Fetch an image and flatten it into a bootable disk.
    Pull(PullArgs),
    /// Image commands.
    #[command(subcommand)]
    Image(ImageCommand),
    /// Create and start a VM.
    Run(Box<RunArgs>),
    /// Run a command in a running VM.
    Exec(ExecArgs),
    /// Copy files between the host and a running VM.
    Cp(CpArgs),
    /// Container (VM) commands.
    #[command(subcommand)]
    Container(ContainerCommand),
    /// Start stopped VMs from their retained disks.
    Start(NamesArgs),
    /// Stop VMs.
    Stop(StopArgs),
    /// Remove VMs.
    Rm(RmArgs),
    /// Volume commands.
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// The guest agent. Runs as PID 1 inside a VM.
    #[command(hide = true)]
    GuestAgent,
    /// Remove a TAP device; run by the VM's systemd unit when it stops.
    #[command(hide = true)]
    HostTapDown { tap: String },
}

#[derive(Debug, Args)]
struct InfoArgs {
    /// Only `json` is supported.
    #[arg(long)]
    format: Option<String>,
}

#[derive(Debug, Args)]
struct PullArgs {
    #[arg(long)]
    platform: Option<String>,
    /// Accepted; the image id is the only output either way.
    #[arg(short, long)]
    quiet: bool,
    image: String,
}

#[derive(Debug, Subcommand)]
enum ImageCommand {
    /// Describe an image.
    Inspect {
        #[arg(long)]
        format: Option<String>,
        image: String,
    },
    /// Remove images no reference or VM uses.
    Prune {
        #[arg(short, long)]
        force: bool,
    },
    /// Exit 0 when the image is known, 1 when not.
    Exists { image: String },
}

#[derive(Debug, Args)]
struct RunArgs {
    #[arg(short, long)]
    detach: bool,
    #[arg(long)]
    name: String,
    #[arg(short, long = "label")]
    labels: Vec<String>,
    #[arg(long)]
    cpus: Option<f64>,
    #[arg(short, long)]
    memory: Option<String>,
    #[arg(short, long = "env")]
    env: Vec<String>,
    #[arg(short, long = "volume")]
    volumes: Vec<String>,
    #[arg(short, long)]
    user: Option<String>,
    #[arg(short, long)]
    workdir: Option<String>,
    #[arg(long)]
    pull: Option<String>,
    #[arg(long)]
    platform: Option<String>,
    /// Accepted: the guest agent is always an init.
    #[arg(long)]
    init: bool,
    /// Accepted when KILL: stopping a VM always kills it.
    #[arg(long)]
    stop_signal: Option<String>,
    /// Accepted with no effect: the VM is the isolation boundary.
    #[arg(long = "cap-add")]
    cap_add: Vec<String>,
    #[arg(long = "cap-drop")]
    cap_drop: Vec<String>,
    #[arg(long = "security-opt")]
    security_opt: Vec<String>,
    /// Accepted with no effect: each VM has its own kernel and process table.
    #[arg(long)]
    pids_limit: Option<String>,
    image: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct ExecArgs {
    #[arg(short, long)]
    interactive: bool,
    #[arg(short, long)]
    detach: bool,
    #[arg(short, long)]
    user: Option<String>,
    #[arg(short, long = "env")]
    env: Vec<String>,
    #[arg(short, long)]
    workdir: Option<String>,
    container: String,
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct CpArgs {
    source: String,
    destination: String,
}

#[derive(Debug, Subcommand)]
enum ContainerCommand {
    /// Exit 0 when the VM exists, 1 when not.
    Exists { name: String },
    /// Describe a VM as a one-element JSON array.
    Inspect {
        #[arg(long)]
        format: Option<String>,
        name: String,
    },
}

#[derive(Debug, Args)]
struct NamesArgs {
    #[arg(required = true)]
    names: Vec<String>,
}

#[derive(Debug, Args)]
struct StopArgs {
    /// Seconds the guest has to flush and power off before the VM is killed.
    #[arg(short, long)]
    time: Option<u64>,
    #[arg(long)]
    ignore: bool,
    #[arg(required = true)]
    names: Vec<String>,
}

#[derive(Debug, Args)]
struct RmArgs {
    #[arg(short, long)]
    force: bool,
    #[arg(long)]
    ignore: bool,
    /// Also remove the VM's named volumes.
    #[arg(short = 'v', long)]
    volumes: bool,
    #[arg(required = true)]
    names: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum VolumeCommand {
    Create {
        #[arg(short, long = "label")]
        labels: Vec<String>,
        name: String,
    },
    Rm {
        /// Ignore volumes that do not exist.
        #[arg(short, long)]
        force: bool,
        #[arg(required = true)]
        names: Vec<String>,
    },
    Ls {
        #[arg(long)]
        format: Option<String>,
    },
    Inspect {
        #[arg(long)]
        format: Option<String>,
        name: String,
    },
    Exists {
        name: String,
    },
}

/// Run the engine with `args` (the words after `microvm`) and return the exit status.
pub fn main(args: Vec<OsString>) -> Result<i32> {
    let argv: Vec<OsString> = std::iter::once(OsString::from("mj-worker microvm"))
        .chain(args)
        .collect();
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(error) => {
            let status = if error.use_stderr() {
                ENGINE_FAILURE
            } else {
                0
            };
            // clap writes its own text, including --help and --version.
            error.print().context("print command-line error")?;
            return Ok(status);
        }
    };
    let layout = Layout::system();
    dispatch(&layout, cli.command)
}

fn dispatch(layout: &Layout, command: Command) -> Result<i32> {
    match command {
        Command::GuestAgent => {
            super::guest_agent::run_guest_agent()?;
            Ok(0)
        }
        Command::HostTapDown { tap } => {
            net::tap_down(&tap)?;
            Ok(0)
        }
        Command::Info(args) => {
            let info = vm::info(layout)?;
            match args.format.as_deref() {
                Some("json") => println!("{}", serde_json::to_string_pretty(&info)?),
                Some(other) => bail!("unsupported --format {other:?} for info: use json"),
                None => print_info(&info),
            }
            Ok(0)
        }
        Command::Pull(args) => {
            let id = image::pull(layout, &args.image, args.platform.as_deref())?;
            println!("{id}");
            Ok(0)
        }
        Command::Image(command) => image_command(layout, command),
        Command::Run(args) => run(layout, *args),
        Command::Exec(args) => exec(layout, args),
        Command::Cp(args) => copy(layout, args),
        Command::Container(ContainerCommand::Exists { name }) => {
            Ok(if vm::exists(layout, &name)? { 0 } else { 1 })
        }
        Command::Container(ContainerCommand::Inspect { format, name }) => {
            let description = vm::inspect(layout, &name)?;
            print_described(&description, format.as_deref())?;
            Ok(0)
        }
        Command::Start(args) => {
            for name in &args.names {
                vm::start(layout, name)?;
                println!("{name}");
            }
            Ok(0)
        }
        Command::Stop(args) => {
            let grace = Duration::from_secs(match args.time {
                Some(0) | None => vm::DEFAULT_STOP_SECONDS,
                Some(seconds) => seconds,
            });
            for name in &args.names {
                vm::stop(layout, name, grace, args.ignore)?;
                println!("{name}");
            }
            Ok(0)
        }
        Command::Rm(args) => {
            for name in &args.names {
                vm::remove(layout, name, args.force, args.ignore, args.volumes)?;
                println!("{name}");
            }
            Ok(0)
        }
        Command::Volume(command) => volume_command(layout, command),
    }
}

fn print_info(info: &Value) {
    if let Some(object) = info.as_object() {
        for (key, value) in object {
            println!("{key}: {value}");
        }
    }
}

/// Print a described object as Podman does: a one-element array, or the
/// `--format` rendering.
fn print_described(description: &Value, format: Option<&str>) -> Result<()> {
    match format {
        None => println!(
            "{}",
            serde_json::to_string_pretty(&Value::Array(vec![description.clone()]))?
        ),
        Some(format) => print!("{}", template::render(format, description)?),
    }
    Ok(())
}

fn image_command(layout: &Layout, command: ImageCommand) -> Result<i32> {
    match command {
        ImageCommand::Inspect { format, image } => {
            let id = image::lookup(layout, &image)?
                .ok_or_else(|| anyhow!("{image}: image not known"))?;
            let config = ImageConfig::read(layout, &id)?;
            let refs = super::state::ImageRefs::read(layout)?;
            let tags: Vec<&String> = refs
                .refs
                .iter()
                .filter(|(_, target)| **target == id)
                .map(|(reference, _)| reference)
                .collect();
            let description = serde_json::json!({
                "Id": id,
                "RepoTags": tags,
                "Architecture": config.architecture,
                "Size": config.root_bytes,
                "Config": {
                    "User": config.user,
                    "Env": config.env,
                    "WorkingDir": config.working_dir,
                    "Entrypoint": config.entrypoint,
                    "Cmd": config.cmd,
                    "Labels": config.labels,
                },
            });
            print_described(&description, format.as_deref())?;
            Ok(0)
        }
        ImageCommand::Prune { force } => {
            ensure!(
                force,
                "image prune needs -f: there is no confirmation prompt"
            );
            for id in image::prune(layout)? {
                println!("{id}");
            }
            Ok(0)
        }
        ImageCommand::Exists { image } => Ok(if image::lookup(layout, &image)?.is_some() {
            0
        } else {
            1
        }),
    }
}

fn parse_memory(text: &str) -> Result<u64> {
    let lower = text.to_ascii_lowercase();
    let (digits, factor) = match lower.trim_end_matches('b') {
        value if value.ends_with('k') => (&value[..value.len() - 1], 1u64 << 10),
        value if value.ends_with('m') => (&value[..value.len() - 1], 1u64 << 20),
        value if value.ends_with('g') => (&value[..value.len() - 1], 1u64 << 30),
        value => (value, 1),
    };
    let amount: u64 = digits.parse().with_context(|| {
        format!("invalid --memory {text:?}: use a number with an optional b, k, m or g suffix")
    })?;
    amount
        .checked_mul(factor)
        .ok_or_else(|| anyhow!("--memory {text:?} is too large"))
}

fn parse_volume(text: &str) -> Result<VolumeArg> {
    let mut parts = text.splitn(3, ':');
    let name = parts.next().unwrap_or_default();
    let destination = parts
        .next()
        .ok_or_else(|| anyhow!("invalid --volume {text:?}: use name:/path[:options]"))?;
    ensure!(
        !name.starts_with('/') && !name.starts_with('.') && !name.starts_with('~'),
        "--volume {text:?} names a host path; Nitro Sandboxes support named volumes only"
    );
    let mut read_write = true;
    let mut chown = false;
    for option in parts
        .next()
        .unwrap_or_default()
        .split(',')
        .filter(|o| !o.is_empty())
    {
        match option {
            "rw" => read_write = true,
            "ro" => read_write = false,
            "U" => chown = true,
            other => bail!(
                "unsupported volume option {other:?} in --volume {text:?}; supported: rw, ro, U"
            ),
        }
    }
    Ok(VolumeArg {
        name: name.to_owned(),
        destination: destination.to_owned(),
        read_write,
        chown,
    })
}

fn parse_labels(labels: &[String]) -> Result<BTreeMap<String, String>> {
    let mut parsed = BTreeMap::new();
    for label in labels {
        let (key, value) = label.split_once('=').unwrap_or((label.as_str(), ""));
        validate_label(key, value)?;
        parsed.insert(key.to_owned(), value.to_owned());
    }
    Ok(parsed)
}

fn run(layout: &Layout, args: RunArgs) -> Result<i32> {
    ensure!(
        args.detach,
        "run without --detach is not supported: a microVM runs in the background and is reached with exec"
    );
    if let Some(signal) = &args.stop_signal {
        ensure!(
            signal.eq_ignore_ascii_case("KILL")
                || signal == "9"
                || signal.eq_ignore_ascii_case("SIGKILL"),
            "unsupported --stop-signal {signal:?}: stopping a VM always kills it; use KILL"
        );
    }
    validate_name("container", &args.name)?;
    let memory = args
        .memory
        .as_deref()
        .map(parse_memory)
        .transpose()?
        .ok_or_else(|| {
            anyhow!("--memory is required: a microVM's memory is reserved from the Nitro pool")
        })?;
    let spec = RunSpec {
        name: args.name,
        image: args.image,
        platform: args.platform,
        pull: args
            .pull
            .as_deref()
            .map(PullPolicy::parse)
            .transpose()?
            .unwrap_or(PullPolicy::Missing),
        labels: parse_labels(&args.labels)?,
        cpus: args.cpus,
        memory_bytes: memory,
        env: args.env,
        user: args.user,
        workdir: args.workdir,
        volumes: args
            .volumes
            .iter()
            .map(|v| parse_volume(v))
            .collect::<Result<_>>()?,
        command: args.command,
    };
    println!("{}", vm::run(layout, &spec)?);
    Ok(0)
}

fn exec(layout: &Layout, args: ExecArgs) -> Result<i32> {
    let (_state, client) = vm::agent_for(layout, &args.container)?;
    let request = ExecRequest {
        argv: args.command,
        env: args.env,
        cwd: args.workdir,
        user: args.user,
        stdin: args.interactive,
        detach: args.detach,
    };
    if args.detach {
        ensure!(
            !args.interactive,
            "--detach and --interactive cannot be combined"
        );
        client.exec_detached(request)?;
        return Ok(0);
    }
    let stdin: Option<Box<dyn Read + Send>> =
        args.interactive.then(|| Box::new(std::io::stdin()) as _);
    let exit = client.exec(
        request,
        stdin,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    )?;
    Ok(exit.shell_status())
}

/// A `cp` operand: `name:path` inside a VM, anything else a host path.
fn split_container_operand(operand: &str) -> Option<(&str, &str)> {
    if operand.starts_with('/') || operand.starts_with('.') {
        return None;
    }
    operand.split_once(':')
}

fn copy(layout: &Layout, args: CpArgs) -> Result<i32> {
    match (
        split_container_operand(&args.source),
        split_container_operand(&args.destination),
    ) {
        (None, Some((name, destination))) => {
            let (_state, client) = vm::agent_for(layout, name)?;
            client.copy_in(&PathBuf::from(&args.source), destination)?;
        }
        (Some((name, source)), None) => {
            let (_state, client) = vm::agent_for(layout, name)?;
            client.copy_out(source, &PathBuf::from(&args.destination), sys::sudo_owner())?;
        }
        (Some(_), Some(_)) => bail!("copying between two containers is not supported"),
        (None, None) => bail!("one of the cp operands must be name:path inside a container"),
    }
    Ok(0)
}

fn volume_command(layout: &Layout, command: VolumeCommand) -> Result<i32> {
    match command {
        VolumeCommand::Create { labels, name } => {
            volume::create(layout, &name, parse_labels(&labels)?)?;
            println!("{name}");
            Ok(0)
        }
        VolumeCommand::Rm { force, names } => {
            for name in &names {
                volume::remove(layout, name, force)?;
                println!("{name}");
            }
            Ok(0)
        }
        VolumeCommand::Ls { format } => {
            layout.ensure()?;
            let volumes: Vec<Value> = VolumeMetadata::list(layout)?
                .iter()
                .map(|metadata| describe_volume(layout, metadata))
                .collect();
            match format.as_deref() {
                None => {
                    println!("DRIVER    VOLUME NAME");
                    for volume in &volumes {
                        println!("local     {}", volume["Name"].as_str().unwrap_or_default());
                    }
                }
                Some("json") => println!("{}", serde_json::to_string_pretty(&volumes)?),
                Some(format) => {
                    for volume in &volumes {
                        print!("{}", template::render(format, volume)?);
                    }
                }
            }
            Ok(0)
        }
        VolumeCommand::Inspect { format, name } => {
            validate_name("volume", &name)?;
            let metadata = VolumeMetadata::read(layout, &name)?
                .ok_or_else(|| anyhow!("no volume with name {name:?} found: no such volume"))?;
            print_described(&describe_volume(layout, &metadata), format.as_deref())?;
            Ok(0)
        }
        VolumeCommand::Exists { name } => {
            validate_name("volume", &name)?;
            Ok(if VolumeMetadata::read(layout, &name)?.is_some() {
                0
            } else {
                1
            })
        }
    }
}

fn describe_volume(layout: &Layout, metadata: &VolumeMetadata) -> Value {
    serde_json::json!({
        "Name": metadata.name,
        "Driver": "local",
        "Mountpoint": layout.volume(&metadata.name),
        "CreatedAt": metadata.created,
        "Labels": metadata.labels,
    })
}
