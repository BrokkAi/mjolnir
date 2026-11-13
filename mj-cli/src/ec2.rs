//! Persistent AWS hosts are ordinary SSH machines running rootless Podman.
//! AWS tags own cloud inventory; the local record owns keys and recovery after
//! an interrupted create. No daemon or dashboard waits on this provisioning.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use clap::{Args, Subcommand, ValueEnum};
use mj_core::config::{
    Config, ContainerTemplate, Machine, SshConnection, TargetBuildCache, TargetTemplate,
    config_dir, config_path, validate_id,
};
use mj_core::subprocess::run_with_input;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const MANAGER: &str = "mj-ec2";
const CACHE_DIRECTORY: &str = "/mnt/nvme/mbx";

#[derive(Debug, Args)]
pub(crate) struct Ec2Args {
    #[command(subcommand)]
    command: Ec2Command,
}

#[derive(Debug, Subcommand)]
enum Ec2Command {
    /// Launch a host (Ubuntu with rootless Podman, or Amazon Linux with Nitro
    /// Sandboxes) and add an SSH machine/target.
    /// Charges continue until you stop or destroy the VM.
    Create(CreateArgs),
    /// List mj-managed VMs and unfinished local provisioning operations.
    List(ListArgs),
    /// Terminate a managed VM and delete its disk, sessions, keys, and target.
    /// Also cleans up an interrupted create; accepts its operation ID.
    Destroy(DestroyArgs),
}

#[derive(Clone, Debug, Default, Args)]
struct AwsArgs {
    /// AWS region. Omitted uses the saved host's region or the AWS default.
    #[arg(long)]
    region: Option<String>,
    /// AWS CLI credential profile (independent of an mj harness profile).
    #[arg(long)]
    aws_profile: Option<String>,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// Instance types in preference order (comma-separated). Also accepts --type.
    #[arg(
        value_delimiter = ',',
        required_unless_present = "types",
        conflicts_with = "types"
    )]
    instance_type: Vec<String>,
    /// Instance types in preference order; repeat or separate with commas.
    #[arg(
        long = "type",
        value_delimiter = ',',
        required_unless_present = "instance_type"
    )]
    types: Vec<String>,
    /// Host runtime to install. Nitro hosts run workloads in isolated microVMs.
    #[arg(long, value_enum, default_value_t = Ec2Runtime::Podman)]
    runtime: Ec2Runtime,
    /// Zones in preference order; repeat or separate with commas.
    /// Listed regions without explicit zones try all available ordinary zones.
    #[arg(long, value_delimiter = ',', required_unless_present = "region")]
    zone: Vec<String>,
    /// Regions in preference order; repeat or separate with commas.
    /// Omitted infers regions from --zone. Tries regions, then types, then zones.
    #[arg(long, value_delimiter = ',')]
    region: Vec<String>,
    /// AWS CLI credential profile (independent of an mj harness profile).
    #[arg(long)]
    aws_profile: Option<String>,
    /// Machine and target name. Omitted generates a unique name.
    #[arg(long)]
    name: Option<String>,
    /// Public subnet in the selected zone. Omitted uses its default subnet.
    #[arg(long)]
    subnet: Option<String>,
    /// Allowed SSH source CIDR. Omitted discovers this controller's public IPv4.
    #[arg(long)]
    ssh_cidr: Option<String>,
    /// Encrypted gp3 root disk size in GiB; separate from local SSDs.
    #[arg(long, default_value_t = 200)]
    disk_gib: u32,
    /// EBS gp3 throughput in MiB/s. Omitted uses the AWS default.
    #[arg(long)]
    disk_mbps: Option<u32>,
    /// EBS gp3 provisioned IOPS. Omitted uses the AWS default.
    #[arg(long)]
    disk_iops: Option<u32>,
    /// Require local NVMe storage (used automatically when available).
    /// ZFS/lz4 holds Podman and mbx; stop/start clears this storage.
    #[arg(long)]
    zfs_cache: bool,
    /// Print the created host as JSON. Progress still goes to stderr.
    #[arg(long)]
    json: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
enum Ec2Runtime {
    #[default]
    Podman,
    Nitro,
}

impl Ec2Runtime {
    fn label(self) -> &'static str {
        match self {
            Self::Podman => "Podman",
            Self::Nitro => "Nitro sandbox",
        }
    }
}

#[derive(Debug, Args)]
struct ListArgs {
    #[command(flatten)]
    aws: AwsArgs,
    /// Print the inventory as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct DestroyArgs {
    /// Managed target name, EC2 instance ID, or interrupted create operation ID.
    host: String,
    #[command(flatten)]
    aws: AwsArgs,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Record {
    operation: String,
    target: String,
    region: String,
    account: String,
    #[serde(default)]
    aws_profile: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    state: RecordState,
    #[serde(default)]
    attempts: Vec<Attempt>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum RecordState {
    Preparing,
    // Older records may already own resources even without a saved instance ID.
    #[default]
    Active,
    CleanedUp,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Attempt {
    region: String,
    zone: Option<String>,
    instance_type: Option<String>,
    outcome: String,
    reason: Option<String>,
}

#[derive(Debug)]
struct AwsFailure {
    service: String,
    operation: String,
    code: Option<String>,
    message: String,
}

impl std::fmt::Display for AwsFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "aws {} {}: {}",
            self.service, self.operation, self.message
        )
    }
}

impl std::error::Error for AwsFailure {}

#[derive(Debug)]
struct PlacementUnavailable(String);

impl std::fmt::Display for PlacementUnavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for PlacementUnavailable {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Refusal {
    Placement,
    Region,
}

fn refusal(error: &anyhow::Error) -> Option<Refusal> {
    if error.is::<PlacementUnavailable>() {
        return Some(Refusal::Placement);
    }
    let failure = error.downcast_ref::<AwsFailure>()?;
    if failure.service != "ec2" {
        return None;
    }
    match failure.code.as_deref()? {
        "OptInRequired" => Some(Refusal::Region),
        "VcpuLimitExceeded" | "InstanceLimitExceeded" if failure.operation == "run-instances" => {
            Some(Refusal::Region)
        }
        "InsufficientInstanceCapacity" if failure.operation == "run-instances" => {
            Some(Refusal::Placement)
        }
        "Unsupported" | "InvalidParameterValue" if failure.operation == "run-instances" => {
            let message = failure.message.to_ascii_lowercase();
            (message.contains("instance type")
                && (message.contains("availability zone") || message.contains("region")))
            .then_some(Refusal::Placement)
        }
        _ => None,
    }
}

impl Record {
    fn directory(&self, root: &Path) -> Result<PathBuf> {
        ensure!(
            self.operation.len() == 32 && self.operation.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid managed EC2 operation ID {:?}",
            self.operation
        );
        Ok(root.join(&self.operation))
    }

    fn save(&self, root: &Path) -> Result<()> {
        mj_core::config::atomic_write(
            &self.directory(root)?.join("resources.json"),
            &serde_json::to_vec_pretty(self)?,
        )
    }
}

struct Cloud {
    options: AwsArgs,
    program: PathBuf,
}

impl Cloud {
    fn new(options: AwsArgs) -> Self {
        Self {
            options,
            program: "aws".into(),
        }
    }

    fn call(&self, service: &str, operation: &[&str], request: Value) -> Result<Value> {
        let mut command = Command::new(&self.program);
        command.args([
            "--no-cli-pager",
            "--output",
            "json",
            "--cli-binary-format",
            "base64",
        ]);
        if let Some(region) = &self.options.region {
            command.args(["--region", region]);
        }
        if let Some(profile) = &self.options.aws_profile {
            command.args(["--profile", profile]);
        }
        command
            .arg(service)
            .args(operation)
            .arg("--cli-input-json")
            .arg(request.to_string());
        let output = run_with_input(&mut command, &[])
            .with_context(|| format!("aws {service} {}", operation.join(" ")))?;
        if !output.status.success() {
            let message = format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            // The AWS CLI's service-error format is stable; an unrecognized
            // response (including a lost acknowledgement) never permits retry.
            let code = String::from_utf8_lossy(&output.stderr)
                .split_once("An error occurred (")
                .and_then(|(_, rest)| rest.split_once(") when calling "))
                .map(|(code, _)| code.to_owned());
            return Err(AwsFailure {
                service: service.into(),
                operation: operation.join(" "),
                code,
                message: format!("{}: {}", output.status, message.trim()),
            }
            .into());
        }
        if output.stdout.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&output.stdout).context("decode AWS CLI JSON response")
    }

    fn ec2(&self, operation: &str, request: Value) -> Result<Value> {
        self.call("ec2", &[operation], request)
    }

    fn instances(&self) -> Result<Vec<Value>> {
        let response = self.ec2(
            "describe-instances",
            json!({"Filters": managed_filters(None)}),
        )?;
        let reservations = response["Reservations"]
            .as_array()
            .context("AWS returned no Reservations")?;
        let mut instances = Vec::new();
        for reservation in reservations {
            instances.extend(
                reservation["Instances"]
                    .as_array()
                    .context("AWS returned no Instances")?
                    .iter()
                    .cloned(),
            );
        }
        Ok(instances)
    }
}

fn checked(command: &mut Command, input: &[u8]) -> Result<Output> {
    let output = run_with_input(command, input)?;
    ensure!(
        output.status.success(),
        "{} exited with {}: {}{}",
        command.get_program().to_string_lossy(),
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn tag(value: &Value, key: &str) -> Option<String> {
    value["Tags"]
        .as_array()?
        .iter()
        .find(|tag| tag["Key"] == key)?["Value"]
        .as_str()
        .map(str::to_owned)
}

fn managed_filters(operation: Option<&str>) -> Vec<Value> {
    let mut filters = vec![json!({"Name":"tag:mj:managed-by","Values":[MANAGER]})];
    if let Some(operation) = operation {
        filters.push(json!({"Name":"tag:mj:operation","Values":[operation]}));
    }
    filters
}

fn tags(record: &Record, resource: &str) -> Value {
    json!({"ResourceType":resource,"Tags":[
        {"Key":"mj:managed-by","Value":MANAGER},
        {"Key":"mj:operation","Value":record.operation},
        {"Key":"mj:target","Value":record.target},
        {"Key":"Name","Value":record.target}
    ]})
}

fn records(root: &Path) -> Result<Vec<Record>> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut records = Vec::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path().join("resources.json");
        if path.is_file() {
            let record: Record = serde_json::from_slice(&fs::read(&path)?)
                .with_context(|| format!("read {}", path.display()))?;
            ensure!(
                record.directory(root)?.join("resources.json") == path,
                "EC2 record has mismatched operation ID"
            );
            records.push(record);
        }
    }
    Ok(records)
}

pub(crate) async fn run(args: Ec2Args) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let root = config_dir().join("ec2");
        match args.command {
            Ec2Command::Create(args) => create(args, &root),
            Ec2Command::List(args) => list(args, &root),
            Ec2Command::Destroy(args) => destroy(args, &root),
        }
    })
    .await
    .context("EC2 provisioning task failed")?
}

fn region_for_zone(zone: &str) -> Result<String> {
    // Also handles GovCloud and Local Zones such as us-west-2-lax-1a.
    let parts: Vec<_> = zone.split('-').collect();
    let end = parts
        .iter()
        .position(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .context("cannot infer region from --zone")?;
    let number = parts[end].trim_end_matches(|c: char| c.is_ascii_alphabetic());
    Ok(format!("{}-{number}", parts[..end].join("-")))
}

fn new_record(
    target: &str,
    region: &str,
    profile: &Option<String>,
    attempts: &[Attempt],
) -> Result<Record> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("generate EC2 operation ID: {e}"))?;
    Ok(Record {
        operation: mj_core::hex::lower_hex(random),
        target: target.into(),
        region: region.into(),
        account: String::new(),
        aws_profile: profile.clone(),
        instance_id: None,
        state: RecordState::Preparing,
        attempts: attempts.to_vec(),
    })
}

fn save_attempt(record: &Record, root: &Path) -> Result<()> {
    let directory = record.directory(root)?;
    fs::create_dir_all(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    record.save(root)?;
    // A completed cleanup owns no keys or cloud resources. Supersede its
    // history only after the replacement has been saved under the target lock.
    for old in records(root)? {
        if old.target == record.target
            && old.operation != record.operation
            && old.state == RecordState::CleanedUp
        {
            fs::remove_dir_all(old.directory(root)?)?;
        }
    }
    Ok(())
}

fn regions_and_zones(args: &CreateArgs) -> Result<Vec<(String, Vec<String>)>> {
    let mut locations: Vec<(String, Vec<String>)> = Vec::new();
    for region in &args.region {
        if !locations.iter().any(|(name, _)| name == region) {
            locations.push((region.clone(), Vec::new()));
        }
    }
    for zone in &args.zone {
        let region = region_for_zone(zone)?;
        if !args.region.is_empty() {
            ensure!(
                args.region.contains(&region),
                "zone {zone} is outside the requested regions"
            );
        }
        let index = match locations.iter().position(|(name, _)| name == &region) {
            Some(index) => index,
            None => {
                locations.push((region, Vec::new()));
                locations.len() - 1
            }
        };
        if !locations[index].1.contains(zone) {
            locations[index].1.push(zone.clone());
        }
    }
    ensure!(!locations.is_empty(), "pass --zone or --region");
    if args.subnet.is_some() {
        ensure!(
            locations.len() == 1 && locations[0].1.len() == 1,
            "--subnet requires exactly one --zone in one region"
        );
    }
    Ok(locations)
}

fn available_zones(cloud: &Cloud) -> Result<Vec<String>> {
    let response = cloud.ec2(
        "describe-availability-zones",
        json!({"Filters":[
            {"Name":"zone-type","Values":["availability-zone"]},
            {"Name":"state","Values":["available"]}
        ]}),
    )?;
    let mut zones: Vec<String> = response["AvailabilityZones"]
        .as_array()
        .context("AWS returned no AvailabilityZones")?
        .iter()
        .map(|zone| {
            zone["ZoneName"]
                .as_str()
                .map(str::to_owned)
                .context("availability zone has no name")
        })
        .collect::<Result<_>>()?;
    zones.sort();
    zones.dedup();
    ensure!(!zones.is_empty(), "AWS returned no available zones");
    Ok(zones)
}

struct Placement<'a> {
    instance_type: &'a str,
    zone: &'a str,
}

fn create(args: CreateArgs, root: &Path) -> Result<()> {
    let locations = regions_and_zones(&args)?;
    ensure!(
        args.runtime != Ec2Runtime::Nitro || !args.zfs_cache,
        "--zfs-cache is not available for Nitro hosts"
    );
    let initial = new_record("", &locations[0].0, &args.aws_profile, &[])?;
    let target = args
        .name
        .clone()
        .unwrap_or_else(|| format!("ec2-{}", &initial.operation[..8]));
    validate_id("machine", &target)?;
    ensure!(target != "local", "local is reserved for this computer");
    fs::create_dir_all(root)?;
    // One owner serializes the entire request, including refusal cleanup.
    let mut lock = Some(lock_target(root, &target)?);
    let config = Config::load()?;
    ensure!(
        !config.machines.contains_key(&target) && !config.targets.contains_key(&target),
        "machine or target {target:?} already exists"
    );
    ensure!(
        !records(root)?
            .iter()
            .any(|record| record.target == target && record.state != RecordState::CleanedUp),
        "an earlier create for {target:?} remains; use mj ec2 list, then destroy its operation before retrying"
    );
    let types = if args.types.is_empty() {
        &args.instance_type
    } else {
        &args.types
    };
    let mut attempts = Vec::new();
    for (region, requested_zones) in locations {
        let cloud = Cloud::new(AwsArgs {
            region: Some(region.clone()),
            aws_profile: args.aws_profile.clone(),
        });
        let region_setup = (|| -> Result<Vec<String>> {
            ensure!(
                !cloud
                    .instances()?
                    .iter()
                    .any(
                        |instance| tag(instance, "mj:target").as_deref() == Some(&target)
                            && instance["State"]["Name"] != "terminated"
                    ),
                "managed EC2 host {target:?} already exists in {region}"
            );
            if requested_zones.is_empty() {
                available_zones(&cloud)
            } else {
                Ok(requested_zones)
            }
        })();
        let zones = match region_setup {
            Ok(zones) => zones,
            Err(error) => {
                let skip = refusal(&error) == Some(Refusal::Region);
                attempts.push(Attempt {
                    region: region.clone(),
                    zone: None,
                    instance_type: None,
                    outcome: if skip { "skipped_region" } else { "failed" }.into(),
                    reason: Some(format!("{error:#}")),
                });
                let mut record = new_record(&target, &region, &args.aws_profile, &attempts)?;
                record.state = RecordState::CleanedUp;
                save_attempt(&record, root)?;
                report_attempts(&attempts[attempts.len() - 1..]);
                if skip {
                    continue;
                }
                return Err(error);
            }
        };
        'types: for instance_type in types {
            for zone in &zones {
                attempts.push(Attempt {
                    region: region.clone(),
                    zone: Some(zone.clone()),
                    instance_type: Some(instance_type.clone()),
                    outcome: "provisioning".into(),
                    reason: None,
                });
                let mut record = new_record(&target, &region, &args.aws_profile, &attempts)?;
                save_attempt(&record, root)?;
                eprintln!("Trying {instance_type} in {zone} ({region}).");
                let result = create_attempt(
                    &args,
                    &Placement {
                        instance_type,
                        zone,
                    },
                    &cloud,
                    &mut record,
                    root,
                    &mut lock,
                );
                match result {
                    Ok(mut host) => {
                        // create_attempt saves the success before releasing its lock.
                        host["attempts"] = serde_json::to_value(&record.attempts)?;
                        if args.json {
                            println!("{}", serde_json::to_string_pretty(&host)?);
                        } else {
                            println!(
                                "Created {} ({}) at {} in {} ({instance_type}, {region}).\n{} target: {}\n{}",
                                record.target,
                                text_field(&host, "instance_id"),
                                text_field(&host, "public_ip"),
                                zone,
                                args.runtime.label(),
                                record.target,
                                cleanup_command(&record)
                            );
                            report_attempts_to_stdout(&record.attempts);
                        }
                        return Ok(());
                    }
                    Err(error) => {
                        let advance = if record.instance_id.is_none() {
                            refusal(&error)
                        } else {
                            None
                        };
                        let last = record.attempts.last_mut().expect("current attempt");
                        last.outcome = if advance == Some(Refusal::Region) {
                            "skipped_region"
                        } else {
                            "failed"
                        }
                        .into();
                        last.reason = Some(format!("{error:#}"));
                        record.save(root)?;
                        report_attempts(&record.attempts[record.attempts.len() - 1..]);
                        if let Some(advance) = advance {
                            if record.state == RecordState::Active {
                                cleanup_record(&cloud, &record, root).with_context(|| {
                                    format!(
                                        "refusal cleanup failed; {}; no further launches attempted",
                                        cleanup_command(&record)
                                    )
                                })?;
                            } else {
                                fs::remove_dir_all(record.directory(root)?)?;
                            }
                            record.state = RecordState::CleanedUp;
                            save_attempt(&record, root)?;
                            attempts = record.attempts;
                            if advance == Refusal::Region {
                                break 'types;
                            }
                        } else {
                            return Err(error).with_context(|| {
                                format!(
                                    "provisioning resources were retained for inspection; {}",
                                    cleanup_command(&record)
                                )
                            });
                        }
                    }
                }
            }
        }
    }
    report_attempts(&attempts);
    bail!(
        "no requested EC2 placement succeeded; all refused attempts were cleaned up (history: {})",
        crate::mj_command("ec2 list")
    )
}

fn attempt_text(attempt: &Attempt) -> String {
    format!(
        "{} / {} / {}: {}{}",
        attempt.region,
        attempt.instance_type.as_deref().unwrap_or("-"),
        attempt.zone.as_deref().unwrap_or("-"),
        attempt.outcome,
        attempt
            .reason
            .as_ref()
            .map(|reason| format!(" — {reason}"))
            .unwrap_or_default()
    )
}

fn report_attempts(attempts: &[Attempt]) {
    for attempt in attempts {
        eprintln!("{}", attempt_text(attempt));
    }
}

fn report_attempts_to_stdout(attempts: &[Attempt]) {
    for attempt in attempts {
        println!("  {}", attempt_text(attempt));
    }
}

fn create_attempt(
    args: &CreateArgs,
    placement: &Placement<'_>,
    cloud: &Cloud,
    record: &mut Record,
    root: &Path,
    lock: &mut Option<mj_core::file_lock::FileLock>,
) -> Result<Value> {
    let offerings = cloud.ec2(
        "describe-instance-type-offerings",
        json!({"LocationType":"availability-zone","Filters":[
            {"Name":"instance-type","Values":[placement.instance_type]},
            {"Name":"location","Values":[placement.zone]}
        ]}),
    )?;
    if offerings["InstanceTypeOfferings"]
        .as_array()
        .context("AWS returned no InstanceTypeOfferings")?
        .is_empty()
    {
        return Err(PlacementUnavailable(format!(
            "{} is not offered in {}",
            placement.instance_type, placement.zone
        ))
        .into());
    }
    let info = cloud.ec2(
        "describe-instance-types",
        json!({"InstanceTypes":[placement.instance_type]}),
    )?;
    let instance_type = info["InstanceTypes"]
        .as_array()
        .and_then(|v| v.first())
        .context("AWS returned no instance type")?;
    if args.runtime == Ec2Runtime::Nitro && instance_type["NitroEnclavesSupport"] != "supported" {
        return Err(PlacementUnavailable(format!(
            "{} does not support Nitro Enclaves",
            placement.instance_type
        ))
        .into());
    }
    let architecture = instance_type["ProcessorInfo"]["SupportedArchitectures"]
        .as_array()
        .context("instance type has no CPU architecture")?;
    let (ami_arch, platform) = if architecture.iter().any(|a| a == "x86_64") {
        ("amd64", "linux/amd64")
    } else if architecture.iter().any(|a| a == "arm64") {
        ("arm64", "linux/arm64")
    } else {
        bail!("instance type does not support Ubuntu amd64 or arm64")
    };
    let local_nvme = args.runtime == Ec2Runtime::Podman
        && instance_type["InstanceStorageInfo"]["TotalSizeInGB"]
            .as_u64()
            .is_some_and(|size| size > 0)
        && instance_type["InstanceStorageInfo"]["NvmeSupport"]
            .as_str()
            .is_some_and(|support| support != "unsupported");
    if args.zfs_cache && !local_nvme {
        return Err(PlacementUnavailable(format!(
            "{} has no local NVMe instance storage for --zfs-cache",
            placement.instance_type
        ))
        .into());
    }
    let subnet = if let Some(subnet) = &args.subnet {
        cloud.ec2("describe-subnets", json!({"SubnetIds":[subnet]}))?
    } else {
        cloud.ec2("describe-subnets", json!({"Filters":[{"Name":"availability-zone","Values":[placement.zone]},{"Name":"default-for-az","Values":["true"]}]}))?
    };
    let subnets = subnet["Subnets"]
        .as_array()
        .context("AWS returned no Subnets")?;
    if subnets.is_empty() && args.subnet.is_none() {
        return Err(PlacementUnavailable(format!(
            "{} has no default subnet; trying the next placement",
            placement.zone
        ))
        .into());
    }
    ensure!(
        subnets.len() == 1,
        "no unique default subnet in {}; pass --subnet",
        placement.zone
    );
    ensure!(
        subnets[0]["AvailabilityZone"] == placement.zone,
        "--subnet is outside --zone"
    );
    let subnet_id = subnets[0]["SubnetId"]
        .as_str()
        .context("subnet has no ID")?;
    let vpc = subnets[0]["VpcId"].as_str().context("subnet has no VPC")?;
    let ssm_arch = if ami_arch == "amd64" {
        "x86_64"
    } else {
        ami_arch
    };
    let parameter_name = match args.runtime {
        Ec2Runtime::Podman => format!(
            "/aws/service/canonical/ubuntu/server/24.04/stable/current/{ami_arch}/hvm/ebs-gp3/ami-id"
        ),
        Ec2Runtime::Nitro => {
            format!("/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-6.18-{ssm_arch}")
        }
    };
    let parameter = cloud.call("ssm", &["get-parameter"], json!({"Name":parameter_name}))?;
    let ami = parameter["Parameter"]["Value"]
        .as_str()
        .context("Ubuntu SSM parameter has no image ID")?;
    let root_device = if args.runtime == Ec2Runtime::Podman {
        let image = cloud.ec2(
            "describe-images",
            json!({"ImageIds":[ami],"Owners":["099720109477"]}),
        )?;
        image["Images"]
            .as_array()
            .and_then(|v| v.first())
            .context("image is not published by Canonical")?["RootDeviceName"]
            .as_str()
            .context("Ubuntu image has no root device")?
            .to_owned()
    } else {
        "/dev/xvda".to_owned()
    };
    let cidr = match &args.ssh_cidr {
        Some(cidr) => cidr.clone(),
        None => {
            let address = reqwest::blocking::get("https://checkip.amazonaws.com")?
                .error_for_status()?
                .text()?;
            let address: std::net::Ipv4Addr = address
                .trim()
                .parse()
                .context("discover controller public IPv4; alternatively pass --ssh-cidr")?;
            format!("{address}/32")
        }
    };
    let identity = cloud.call("sts", &["get-caller-identity"], json!({}))?;
    let account = identity["Account"]
        .as_str()
        .context("AWS identity has no account ID")?
        .to_owned();
    record.account = account;
    record.state = RecordState::Active;
    record.save(root)?;
    let directory = record.directory(root)?;
    eprintln!(
        "Creating {}. Recovery/cleanup: {}",
        record.target,
        cleanup_command(record)
    );
    let key = directory.join("id_ed25519");
    checked(
        Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key),
        &[],
    )?;
    let key_name = format!("mj-ec2-{}", record.operation);
    cloud.ec2("import-key-pair", json!({"KeyName":key_name,"PublicKeyMaterial":base64::engine::general_purpose::STANDARD.encode(fs::read(directory.join("id_ed25519.pub"))?),"TagSpecifications":[tags(record,"key-pair")]}))?;
    let group = cloud.ec2("create-security-group", json!({"GroupName":key_name,"Description":format!("Mjolnir {} host {}", if args.runtime == Ec2Runtime::Nitro { "Nitro" } else { "Podman" },record.target),"VpcId":vpc,"TagSpecifications":[tags(record,"security-group")]}))?;
    let group_id = group["GroupId"]
        .as_str()
        .context("AWS returned no security group ID")?;
    cloud.ec2("authorize-security-group-ingress", json!({"GroupId":group_id,"IpPermissions":[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"IpRanges":[{"CidrIp":cidr}]}]}))?;
    let bootstrap = match args.runtime {
        Ec2Runtime::Podman => bootstrap(local_nvme),
        Ec2Runtime::Nitro => nitro_bootstrap(ami_arch),
    };
    let mut ebs = json!({
        "VolumeSize": args.disk_gib,
        "VolumeType": "gp3",
        "Encrypted": true,
        "DeleteOnTermination": true,
    });
    // AWS owns gp3 limits and the size/IOPS/throughput ratios, including
    // regional differences; omitted performance settings use its defaults.
    if let Some(mbps) = args.disk_mbps {
        ebs["Throughput"] = mbps.into();
    }
    if let Some(iops) = args.disk_iops {
        ebs["Iops"] = iops.into();
    }
    let mut launch_request = json!({
        "ImageId":ami,"InstanceType":placement.instance_type,"MinCount":1,"MaxCount":1,
        "KeyName":key_name,"ClientToken":record.operation,"Placement":{"AvailabilityZone":placement.zone},
        "NetworkInterfaces":[{"DeviceIndex":0,"SubnetId":subnet_id,"Groups":[group_id],"AssociatePublicIpAddress":true,"DeleteOnTermination":true}],
        "BlockDeviceMappings":[{"DeviceName":root_device,"Ebs":ebs}],
        "MetadataOptions":{"HttpTokens":"required","HttpEndpoint":"enabled"},
        "UserData":base64::engine::general_purpose::STANDARD.encode(bootstrap),
        "TagSpecifications":[tags(record,"instance"),tags(record,"volume")]
    });
    if args.runtime == Ec2Runtime::Nitro {
        launch_request["EnclaveOptions"] = json!({"Enabled":true});
    }
    let launched = cloud.ec2("run-instances", launch_request)?;
    let id = launched["Instances"][0]["InstanceId"]
        .as_str()
        .context("AWS launch response has no instance ID")?
        .to_owned();
    record.instance_id = Some(id.clone());
    record.save(root)?;
    eprintln!("Launched {id}; waiting for EC2 status checks and cloud-init.");
    cloud.call(
        "ec2",
        &["wait", "instance-status-ok"],
        json!({"InstanceIds":[id]}),
    )?;
    let live = cloud.ec2("describe-instances", json!({"InstanceIds":[id]}))?;
    let host = live["Reservations"][0]["Instances"][0]["PublicIpAddress"]
        .as_str()
        .context("instance has no public IPv4; use a public subnet")?;
    let ssh = if args.runtime == Ec2Runtime::Nitro {
        connection_for_user(record, root, host, "ec2-user")?
    } else {
        connection(record, root, host)?
    };
    mj_controller::controller::wait_for_ec2_ssh(
        &mj_controller::targets::ProcessExecutor,
        &(&ssh).into(),
    )?;
    // A failed bootstrap reports its own log rather than a later symptom.
    const CLOUD_INIT_WAIT: &str = "cloud-init status --wait; status=$?; if [ \"$status\" -ne 0 ]; then sudo tail -100 /var/log/cloud-init-output.log >&2; exit \"$status\"; fi";
    let verify_command = if args.runtime == Ec2Runtime::Nitro {
        // The bootstrap puts the enclave memory pool on the kernel command
        // line; it takes effect at the next boot. The reboot is issued here,
        // not by the bootstrap, so the wait can tell the new boot from the
        // old one instead of racing the shutdown.
        let output = checked(
            &mut ssh_command(
                &ssh,
                &format!("{CLOUD_INIT_WAIT}; cat /proc/sys/kernel/random/boot_id"),
            ),
            &[],
        )?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let boot_id = stdout
            .lines()
            .last()
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .context("the host did not report its boot ID")?;
        checked(
            &mut ssh_command(&ssh, "sudo systemctl --no-block reboot"),
            &[],
        )?;
        eprintln!("Rebooting {id} to reserve the Nitro Enclaves memory pool.");
        mj_controller::controller::wait_for_ec2_reboot(
            &mj_controller::targets::ProcessExecutor,
            &(&ssh).into(),
            boot_id,
        )?;
        "set -eu; version=$(uname -r | cut -d- -f1); printf '%s\\n' \"$version\" | awk -F. '{ if (($1*1000000+$2*1000+$3) < (6*1000000+18*1000+51)) exit 1 }' || { echo 'Nitro host kernel must be at least 6.18.51' >&2; exit 1; }; sudo dmesg | grep -F 'nitro_enclaves: CMA pool' || { echo 'Nitro Enclaves CMA pool was not initialized' >&2; exit 1; }".to_owned()
    } else {
        format!("{CLOUD_INIT_WAIT}; podman info --format '{{{{.Host.Security.Rootless}}}}'")
    };
    let output = checked(&mut ssh_command(&ssh, &verify_command), &[])?;
    if args.runtime == Ec2Runtime::Podman {
        ensure!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "true"),
            "Podman did not report rootless operation"
        );
    }
    // Attach the persistent address after cloud-init's package downloads
    // finish, before publishing a host that can accept session launches.
    let address = cloud.ec2(
        "allocate-address",
        json!({
            "Domain":"vpc", "TagSpecifications":[tags(record,"elastic-ip")]
        }),
    )?;
    let allocation = address["AllocationId"]
        .as_str()
        .context("AWS returned no Elastic IP allocation ID")?;
    let host = address["PublicIp"]
        .as_str()
        .context("AWS returned no Elastic IP address")?;
    cloud.ec2(
        "associate-address",
        json!({"InstanceId":id,"AllocationId":allocation}),
    )?;
    let ssh = if args.runtime == Ec2Runtime::Nitro {
        connection_for_user(record, root, host, "ec2-user")?
    } else {
        connection(record, root, host)?
    };
    mj_controller::controller::wait_for_ec2_ssh(
        &mj_controller::targets::ProcessExecutor,
        &(&ssh).into(),
    )?;
    let cache = local_nvme.then(|| TargetBuildCache {
        directory: Some(CACHE_DIRECTORY.into()),
        ..Default::default()
    });
    let machine = Machine::Ssh {
        ssh: ssh.clone(),
        workspace_prefix: mj_core::config::DEFAULT_WORKSPACE_PREFIX.into(),
        build_cache: cache,
    };
    if local_nvme && args.runtime == Ec2Runtime::Podman {
        eprintln!("Installing mj's pinned mbx on the new host.");
        mj_controller::controller::install_mbx(&machine, &mj_controller::targets::ProcessExecutor)?;
    }
    let image = mj_core::config::DEFAULT_CONTAINER_IMAGE;
    Config::update(|config| {
        ensure!(
            !config.machines.contains_key(&record.target)
                && !config.targets.contains_key(&record.target),
            "machine or target {:?} was added concurrently; the VM is retained",
            record.target
        );
        config
            .machines
            .insert(record.target.clone(), machine.clone());
        let container = ContainerTemplate {
            image: image.into(),
            pull_policy: Default::default(),
            platform: Some(platform.into()),
            cpus: None,
            memory: None,
            environment: Default::default(),
            workspace_storage: Default::default(),
            build_cache: None,
        };
        let ssh = ssh.clone();
        config.targets.insert(
            record.target.clone(),
            match args.runtime {
                Ec2Runtime::Podman => TargetTemplate::SshPodman { ssh, container },
                Ec2Runtime::Nitro => TargetTemplate::SshNitro { ssh, container },
            },
        );
        // A configured disabled kind suppresses runtime discovery, so a
        // Podman or Nitro host never acquires a second, usable bare target.
        let raw = mj_core::config::unique_config_id(
            &config.targets,
            &format!("{}-raw", record.target.chars().take(60).collect::<String>()),
        );
        let Machine::Ssh {
            ssh,
            workspace_prefix,
            ..
        } = &machine
        else {
            unreachable!("new EC2 hosts are SSH machines")
        };
        config.targets.insert(
            raw.clone(),
            TargetTemplate::SshBare {
                ssh: ssh.clone(),
                workspace_prefix: workspace_prefix.clone(),
                permissions: mj_core::config::PermissionMode::Guardian,
            },
        );
        config.disabled_targets.insert(raw);
        Ok(())
    })?;
    // Provisioning is complete. Image warming owns only its image gate,
    // so destroying the registered host never waits for this download.
    record.attempts.last_mut().expect("current attempt").outcome = "created".into();
    record.save(root)?;
    drop(lock.take());
    eprintln!(
        "Registered {} target {}. Preloading {image}.",
        args.runtime.label(),
        record.target
    );
    let image_host = match args.runtime {
        Ec2Runtime::Podman => mj_core::targets::ImageHost::SshPodman((&ssh).into()),
        Ec2Runtime::Nitro => mj_core::targets::ImageHost::SshNitro((&ssh).into()),
    };
    let preload = mj_core::targets::image_refresh(
        image_host,
        image,
        Some(platform),
        mj_core::config::ImagePullPolicy::Missing,
    )
    .context("prepare EC2 dev image preload")
    .and_then(|refresh| {
        mj_controller::pollers::refresh_host_image(
            &refresh,
            &mj_controller::targets::ProcessExecutor,
            &|_| {},
        )
        .map(|_| ())
    });
    if let Err(error) = preload {
        eprintln!(
            "Warning: dev image predownload failed: {error:#}\nThe host is registered; the background image refresher can retry."
        );
    }
    Ok(
        json!({"instance_id":id,"target":record.target,"region":record.region,"zone":placement.zone,"instance_type":placement.instance_type,"public_ip":host,"zfs_cache":local_nvme,"operation":record.operation}),
    )
}

fn cleanup_command(record: &Record) -> String {
    let mut command = crate::mj_command(&format!(
        "ec2 destroy {} --region {}",
        record.operation, record.region
    ));
    if let Some(profile) = &record.aws_profile {
        command.push_str(&format!(
            " --aws-profile {}",
            mj_core::targets::posix_quote(profile)
        ));
    }
    command
}

fn lock_target(root: &Path, target: &str) -> Result<mj_core::file_lock::FileLock> {
    validate_id("EC2 target", target)?;
    let locks = root.join("locks");
    fs::create_dir_all(&locks)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(locks.join(format!("{target}.lock")))?;
    file.lock()?;
    Ok(mj_core::file_lock::FileLock::new(file))
}

fn connection(record: &Record, root: &Path, host: &str) -> Result<SshConnection> {
    connection_for_user(record, root, host, "ubuntu")
}

fn connection_for_user(
    record: &Record,
    root: &Path,
    host: &str,
    user: &str,
) -> Result<SshConnection> {
    let directory = record.directory(root)?;
    Ok(SshConnection {
        host: host.into(),
        user: Some(user.into()),
        identity_file: Some(directory.join("id_ed25519")),
        extra_args: vec![
            "-o".into(),
            "IdentitiesOnly=yes".into(),
            "-o".into(),
            "StrictHostKeyChecking=accept-new".into(),
            "-o".into(),
            format!(
                "UserKnownHostsFile={}",
                directory.join("known_hosts").display()
            ),
        ],
    })
}

fn ssh_command(ssh: &SshConnection, script: &str) -> Command {
    let mut command = Command::new("ssh");
    command.args(["-o", "BatchMode=yes"]);
    command.args(mj_core::targets::ssh_args_with_identity(
        &ssh.extra_args,
        ssh.identity_file.as_deref(),
    ));
    command.arg(format!(
        "{}@{}",
        ssh.user.as_deref().unwrap_or("ubuntu"),
        ssh.host
    ));
    command.arg(script);
    command
}

fn inventory(args: &ListArgs, root: &Path) -> Result<(Vec<Value>, bool)> {
    let saved = records(root)?;
    let mut regions = std::collections::BTreeSet::new();
    if let Some(region) = &args.aws.region {
        regions.insert(Some(region.clone()));
    } else {
        for record in &saved {
            if record.aws_profile == args.aws.aws_profile {
                regions.insert(Some(record.region.clone()));
                regions.extend(
                    record
                        .attempts
                        .iter()
                        .map(|attempt| Some(attempt.region.clone())),
                );
            }
        }
        if regions.is_empty() {
            regions.insert(None);
        }
    }
    let mut rows = Vec::new();
    let mut complete = true;
    for region in regions {
        let cloud = Cloud::new(AwsArgs {
            region: region.clone(),
            ..args.aws.clone()
        });
        match cloud.instances() {
            Ok(instances) => {
                for instance in instances {
                    rows.push(json!({"instance_id":instance["InstanceId"],"target":tag(&instance,"mj:target"),"operation":tag(&instance,"mj:operation"),"state":instance["State"]["Name"],"instance_type":instance["InstanceType"],"zone":instance["Placement"]["AvailabilityZone"],"public_ip":instance["PublicIpAddress"],"region":region}));
                }
            }
            Err(error) => {
                // A region not opted in must not hide hosts in other regions.
                // Other inventory errors remain visible and fail the command.
                if refusal(&error) != Some(Refusal::Region) {
                    complete = false;
                }
                rows.push(json!({"region":region,"state":"inventory unavailable","error":format!("{error:#}")}));
            }
        }
    }
    for record in saved {
        if args
            .aws
            .region
            .as_deref()
            .is_some_and(|region| region != record.region)
            || args.aws.aws_profile != record.aws_profile
        {
            continue;
        }
        let index = if let Some(index) = rows
            .iter()
            .position(|row| row["operation"] == record.operation)
        {
            index
        } else {
            rows.push(json!({"instance_id":record.instance_id,"target":record.target,"operation":record.operation,"state": if record.state == RecordState::CleanedUp { "attempts cleaned up" } else { "incomplete or no longer in AWS inventory" },"region":record.region}));
            rows.len() - 1
        };
        rows[index]["attempts"] = serde_json::to_value(&record.attempts)?;
    }
    Ok((rows, complete))
}

fn list(args: ListArgs, root: &Path) -> Result<()> {
    let (rows, complete) = inventory(&args, root)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("TARGET\tINSTANCE\tTYPE\tZONE\tREGION\tSTATE\tPUBLIC IP\tOPERATION");
        for row in &rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                text_field(row, "target"),
                text_field(row, "instance_id"),
                text_field(row, "instance_type"),
                text_field(row, "zone"),
                text_field(row, "region"),
                text_field(row, "state"),
                text_field(row, "public_ip"),
                text_field(row, "operation")
            );
            if let Some(error) = row["error"].as_str() {
                println!("  {error}");
            }
            if let Some(attempts) = row["attempts"].as_array() {
                for attempt in attempts {
                    println!("  {}", attempt_text(&Attempt::deserialize(attempt)?));
                }
            }
        }
    }
    ensure!(
        complete,
        "some regions could not be inventoried; see inventory errors above"
    );
    Ok(())
}

fn text_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("-")
}

fn destroy(args: DestroyArgs, root: &Path) -> Result<()> {
    let mut saved: Vec<_> = records(root)?
        .into_iter()
        .filter(|r| {
            r.target == args.host
                || r.operation == args.host
                || r.instance_id.as_deref() == Some(&args.host)
        })
        .collect();
    ensure!(
        saved.len() <= 1,
        "multiple provisioning records match {}; use an operation ID",
        args.host
    );
    let saved_lock = if let Some(record) = saved.first_mut() {
        let lock = lock_target(root, &record.target)?;
        let directory = record.directory(root)?;
        *record = serde_json::from_slice(&fs::read(directory.join("resources.json"))?)?;
        if record.state != RecordState::Active {
            fs::remove_dir_all(directory)?;
            println!("Destroyed {}.", record.target);
            return Ok(());
        }
        Some(lock)
    } else {
        None
    };
    let mut cloud = Cloud::new(args.aws.clone());
    if let Some(record) = saved.first() {
        if cloud.options.region.is_none() {
            cloud.options.region = Some(record.region.clone());
        }
        if cloud.options.aws_profile.is_none() {
            cloud.options.aws_profile.clone_from(&record.aws_profile);
        }
        ensure!(
            cloud.options.region.as_deref() == Some(&record.region)
                && cloud.options.aws_profile == record.aws_profile,
            "AWS region/profile differs from the saved host; use the recorded region/profile"
        );
    }
    let instances = cloud.instances()?;
    let matches: Vec<_> = instances
        .iter()
        .filter(|i| {
            if let Some(record) = saved.first() {
                return tag(i, "mj:operation").as_deref() == Some(&record.operation);
            }
            i["InstanceId"] == args.host
                || tag(i, "mj:target").as_deref() == Some(&args.host)
                || tag(i, "mj:operation").as_deref() == Some(&args.host)
        })
        .collect();
    let live: Vec<_> = matches
        .iter()
        .copied()
        .filter(|i| i["State"]["Name"] != "terminated")
        .collect();
    let matches = if live.is_empty() { matches } else { live };
    ensure!(
        matches.len() <= 1,
        "multiple VMs match {}; use an instance or operation ID",
        args.host
    );
    let identity = cloud.call("sts", &["get-caller-identity"], json!({}))?;
    let account = identity["Account"]
        .as_str()
        .context("AWS identity has no account ID")?;
    let record = if let Some(record) = saved.first() {
        ensure!(
            record.account == account,
            "AWS account {account} differs from the host's account {}; select its original credentials",
            record.account
        );
        record.clone()
    } else if let Some(instance) = matches.first() {
        Record {
            account: account.to_owned(),
            operation: tag(instance, "mj:operation")
                .context("managed instance has no operation tag")?,
            target: tag(instance, "mj:target").context("managed instance has no target tag")?,
            region: cloud
                .options
                .region
                .clone()
                .context("pass --region when destroying a host with no local record")?,
            aws_profile: cloud.options.aws_profile.clone(),
            instance_id: instance["InstanceId"].as_str().map(str::to_owned),
            state: RecordState::Active,
            attempts: Vec::new(),
        }
    } else {
        bail!(
            "no mj-managed host or local provisioning operation matches {:?}",
            args.host
        )
    };
    let directory = record.directory(root)?;
    let _lock = match saved_lock {
        Some(lock) => lock,
        None => lock_target(root, &record.target)?,
    };
    let record = if directory.join("resources.json").is_file() {
        serde_json::from_slice::<Record>(&fs::read(directory.join("resources.json"))?)?
    } else {
        record
    };
    if record.state == RecordState::CleanedUp || record.state == RecordState::Preparing {
        fs::remove_dir_all(&directory)?;
    } else {
        cleanup_record(&cloud, &record, root)?;
    }
    println!("Destroyed {}.", record.target);
    Ok(())
}

// Caller owns the target lock. Keys/config disappear only after cloud cleanup.
fn cleanup_record(cloud: &Cloud, record: &Record, root: &Path) -> Result<()> {
    let directory = record.directory(root)?;
    let instances = cloud.instances()?;
    let matches: Vec<_> = instances
        .iter()
        .filter(|instance| tag(instance, "mj:operation").as_deref() == Some(&record.operation))
        .collect();
    ensure!(
        matches.len() <= 1,
        "operation {} owns more than one VM; inspect its AWS tags before destroying it",
        record.operation
    );
    eprintln!(
        "Destroying {} and its sessions, root disk, and owned cloud resources.",
        record.target
    );
    if let Some(instance) = matches.first() {
        ensure!(
            tag(instance, "mj:operation").as_deref() == Some(&record.operation),
            "instance ownership does not match provisioning record"
        );
        if instance["State"]["Name"] != "terminated" {
            cloud.ec2(
                "terminate-instances",
                json!({"InstanceIds":[instance["InstanceId"]]}),
            )?;
            cloud.call(
                "ec2",
                &["wait", "instance-terminated"],
                json!({"InstanceIds":[instance["InstanceId"]]}),
            )?;
        }
    } else if let Some(id) = &record.instance_id {
        // A saved ID disappearing from tagged inventory does not authorize
        // deleting its key or config while an untagged VM might still run.
        let response = cloud.ec2(
            "describe-instances",
            json!({"Filters":[{"Name":"instance-id","Values":[id]}]}),
        )?;
        ensure!(
            response["Reservations"]
                .as_array()
                .is_some_and(Vec::is_empty),
            "saved instance {id} exists without expected ownership tags; restore its tags before destroying it"
        );
    }
    let addresses = cloud.ec2(
        "describe-addresses",
        json!({"Filters":managed_filters(Some(&record.operation))}),
    )?;
    for address in addresses["Addresses"]
        .as_array()
        .context("AWS returned no Addresses")?
    {
        if let Some(association) = address["AssociationId"].as_str() {
            cloud.ec2("disassociate-address", json!({"AssociationId":association}))?;
        }
        cloud.ec2(
            "release-address",
            json!({"AllocationId":address["AllocationId"]}),
        )?;
    }
    let filters = managed_filters(Some(&record.operation));
    let groups = cloud.ec2("describe-security-groups", json!({"Filters":filters}))?;
    for group in groups["SecurityGroups"]
        .as_array()
        .context("AWS returned no SecurityGroups")?
    {
        cloud.ec2("delete-security-group", json!({"GroupId":group["GroupId"]}))?;
    }
    let keys = cloud.ec2(
        "describe-key-pairs",
        json!({"Filters":managed_filters(Some(&record.operation))}),
    )?;
    for key in keys["KeyPairs"]
        .as_array()
        .context("AWS returned no KeyPairs")?
    {
        cloud.ec2("delete-key-pair", json!({"KeyPairId":key["KeyPairId"]}))?;
    }
    remove_configuration(record, root, &config_path())?;
    if directory.exists() {
        fs::remove_dir_all(&directory)
            .with_context(|| format!("remove local EC2 keys {}", directory.display()))?;
    }
    Ok(())
}

fn remove_configuration(record: &Record, root: &Path, path: &Path) -> Result<()> {
    let key = record.directory(root)?.join("id_ed25519");
    Config::update_to(path, |config| {
        let ours = matches!(config.machines.get(&record.target),Some(Machine::Ssh { ssh,.. }) if ssh.identity_file.as_ref() == Some(&key));
        if ours {
            let targets: Vec<_> = config
                .machine_runtimes(&record.target)
                .map(|(id, _)| id.to_owned())
                .collect();
            for target in targets {
                config.targets.remove(&target);
                config.disabled_targets.remove(&target);
            }
            config.machines.remove(&record.target);
        }
        Ok(())
    })?;
    Ok(())
}

fn bootstrap(zfs_cache: bool) -> String {
    let zfs = if zfs_cache {
        include_str!("ec2_zfs.sh")
    } else {
        ""
    };
    format!(
        r#"#!/bin/bash
set -euxo pipefail
{ssh_setup}
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y podman uidmap slirp4netns fuse-overlayfs dbus-user-session git curl ca-certificates mold {packages}
loginctl enable-linger ubuntu
systemctl start user@1000.service
{zfs}
sudo -u ubuntu XDG_RUNTIME_DIR=/run/user/1000 podman info --format '{{{{.Host.Security.Rootless}}}}'
"#,
        ssh_setup = include_str!("ec2_sshd.sh"),
        packages = if zfs_cache { "zfsutils-linux" } else { "" }
    )
}

fn nitro_bootstrap(architecture: &str) -> String {
    let release_arch = if architecture == "arm64" {
        "aarch64"
    } else {
        "x86_64"
    };
    // Crane hashes are from the checksums.txt release asset for google/go-containerregistry v0.22.1.
    // Firecracker hashes are from the matching firecracker-v1.18.0-<arch>.tgz.sha256.txt assets.
    let (crane_hash, firecracker_hash) = if architecture == "arm64" {
        (
            "898c0cff975f898a33e8c4580bdafb0e7c02c7faa33374e946762f97c4ab7110",
            "2a6130ff0338901ff9d7fbd73ce046c6b1c6123889dc84a34d678aa53bd0c0c2",
        )
    } else {
        (
            "0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0",
            "1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd",
        )
    };
    let ssh_setup = include_str!("ec2_sshd.sh");
    format!(
        r#"#!/bin/bash
set -euxo pipefail
{ssh_setup}
# AL2023 ships curl-minimal, which provides curl and conflicts with the curl package.
dnf install -y nftables e2fsprogs dracut git tar
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
curl -fL --retry 3 -o "$tmp/firecracker.tgz" https://github.com/firecracker-microvm/firecracker/releases/download/v1.18.0/firecracker-v1.18.0-{release_arch}.tgz
echo '{firecracker_hash}  '"$tmp/firecracker.tgz" | sha256sum -c -
tar -xzf "$tmp/firecracker.tgz" -C "$tmp"
install -m 0755 "$tmp/release-v1.18.0-{release_arch}/firecracker-v1.18.0-{release_arch}" /usr/local/bin/firecracker
install -m 0755 "$tmp/release-v1.18.0-{release_arch}/jailer-v1.18.0-{release_arch}" /usr/local/bin/jailer
curl -fL --retry 3 -o "$tmp/crane.tgz" https://github.com/google/go-containerregistry/releases/download/v0.22.1/go-containerregistry_Linux_{crane_arch}.tar.gz
echo '{crane_hash}  '"$tmp/crane.tgz" | sha256sum -c -
tar -xzf "$tmp/crane.tgz" -C "$tmp" crane
install -m 0755 "$tmp/crane" /usr/local/bin/crane
# Takes effect at the next boot; `mj ec2 create` reboots once cloud-init finishes.
grubby --update-kernel=ALL --args="nitro_enclaves.mempool=95%-1G"
"#,
        crane_arch = if architecture == "arm64" {
            "arm64"
        } else {
            "x86_64"
        },
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use clap::Parser;

    // A subprocess fixture checks the AWS CLI's actual argv/JSON boundary and
    // failure ordering. It never reads real credentials or contacts AWS.
    const AWS_FIXTURE: &str = r#"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
service = next(a for a in args if a in ('ec2', 'ssm', 'sts'))
pos = args.index(service)
op = args[pos+1]
if op == 'wait': op += ' ' + args[pos+2]
request = json.loads(args[args.index('--cli-input-json')+1])
with (root/'calls').open('a') as out: out.write(json.dumps({'operation':op,'request':request,'args':args})+'\n')
region = args[args.index('--region')+1]
resources = root/region if (root/'multi-region').exists() else root
resources.mkdir(exist_ok=True)
if not (root/'multi-region').exists(): assert region == 'us-east-1'
if (root/'errors.json').exists():
    for error in json.loads((root/'errors.json').read_text()):
        if error['operation'] != op: continue
        if error.get('region', region) != region: continue
        if error.get('instance_type', request.get('InstanceType')) != request.get('InstanceType'): continue
        if error.get('zone', request.get('Placement',{}).get('AvailabilityZone')) != request.get('Placement',{}).get('AvailabilityZone'): continue
        sys.exit('An error occurred ('+error['code']+') when calling the '+op+' operation: '+error['message'])
assert args[args.index('--cli-binary-format')+1] == 'base64'
response = {}
state = resources/'instance.json'
if op == 'get-caller-identity': response = {'Account':(root/'account').read_text() if (root/'account').exists() else '123456789012'}
elif op == 'describe-instances':
    instance = json.loads(state.read_text()) if state.exists() else None
    instances = [instance] if instance else []
    if (root/'historical').exists() and instance:
        import copy
        old=copy.deepcopy(instance);old['InstanceId']='i-old';old['State']['Name']='terminated'
        next(t for t in old['Tags'] if t['Key']=='mj:operation')['Value']='f'*32
        instances.append(old)
    response = {'Reservations':[{'Instances':instances}] if instances else [], 'Padding':'x'*70000}
elif op == 'describe-availability-zones':
    response = {'AvailabilityZones':[{'ZoneName':region+'b'}, {'ZoneName':region+'a'}]}
elif op == 'describe-instance-type-offerings':
    filters = {f['Name']:f['Values'][0] for f in request['Filters']}
    absent = json.loads((root/'unoffered.json').read_text()) if (root/'unoffered.json').exists() else []
    response = {'InstanceTypeOfferings':[] if [region, filters['instance-type'], filters['location']] in absent else [{'InstanceType':filters['instance-type'], 'Location':filters['location']}]}
elif op == 'describe-instance-types':
    kind = {'ProcessorInfo':{'SupportedArchitectures':['x86_64']}}
    if (root/'nitro').exists(): kind['NitroEnclavesSupport'] = 'supported'
    if (root/'nvme').exists(): kind['InstanceStorageInfo'] = {'TotalSizeInGB':3800,'NvmeSupport':'required'}
    response = {'InstanceTypes':[kind]}
elif op == 'describe-subnets':
    zone = next((f['Values'][0] for f in request.get('Filters',[]) if f['Name']=='availability-zone'), region+'a')
    missing = json.loads((root/'missing-subnets.json').read_text()) if (root/'missing-subnets.json').exists() else []
    response = {'Subnets':[] if zone in missing else [{'SubnetId':'subnet-test','VpcId':'vpc-test','AvailabilityZone':zone}]}
elif op == 'get-parameter':
    if (root/'nitro').exists(): assert request['Name'] == '/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-6.18-x86_64'
    else: assert request['Name'] == '/aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id'
    response = {'Parameter':{'Value':'ami-test'}}
elif op == 'describe-images':
    assert request['Owners'] == ['099720109477']
    response = {'Images':[{'RootDeviceName':'/dev/sda1'}]}
elif op == 'import-key-pair':
    import base64
    assert not (resources/'key.json').exists() and not (resources/'group.json').exists(), 'previous attempt was not cleaned before retry'
    assert base64.b64decode(request['PublicKeyMaterial']).startswith(b'ssh-ed25519 ')
    (resources/'key.json').write_text(json.dumps({'KeyPairId':'key-test','Tags':request['TagSpecifications'][0]['Tags']}))
elif op == 'create-security-group':
    (resources/'group.json').write_text(json.dumps({'GroupId':'sg-test','Tags':request['TagSpecifications'][0]['Tags']}))
    response = {'GroupId':'sg-test'}
elif op == 'run-instances':
    assert request['MinCount'] == request['MaxCount'] == 1
    assert request['MetadataOptions']['HttpTokens'] == 'required'
    assert request['BlockDeviceMappings'][0]['Ebs']['DeleteOnTermination'] is True
    assert request['BlockDeviceMappings'][0]['Ebs']['Encrypted'] is True
    ebs = request['BlockDeviceMappings'][0]['Ebs']
    if (root/'ebs-settings').exists():
        expected = json.loads((root/'ebs-settings').read_text())
        assert {key:ebs[key] for key in expected} == expected
    else:
        assert ebs['VolumeSize'] == 200
        assert 'Throughput' not in ebs and 'Iops' not in ebs
    import base64
    bootstrap = base64.b64decode(request['UserData']).decode()
    assert '/etc/ssh/sshd_config.d/60-mj-mux.conf' in bootstrap
    assert 'MaxSessions 200\nMaxStartups 100:30:300' in bootstrap
    assert 'sshd -t\nsystemctl reload "$ssh_unit"' in bootstrap
    assert bootstrap.index('systemctl start "$ssh_unit"') < bootstrap.index('cat > /etc/ssh/sshd_config.d/60-mj-mux.conf')
    assert 'Include /etc/ssh/sshd_config.d/*.conf' in bootstrap
    if (root/'nitro').exists():
        assert request['EnclaveOptions'] == {'Enabled':True}
        assert 'dnf install -y nftables e2fsprogs dracut git tar\n' in bootstrap
        assert '\nreboot' not in bootstrap
        assert 'nitro_enclaves.mempool=95%-1G' in bootstrap
        assert 'firecracker-v1.18.0-x86_64.tgz' in bootstrap
        assert 'release-v1.18.0-x86_64/firecracker-v1.18.0-x86_64' in bootstrap
        assert '1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd' in bootstrap
        assert 'go-containerregistry_Linux_x86_64.tar.gz' in bootstrap
        assert '0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0' in bootstrap
        assert 'podman' not in bootstrap.lower() and 'zfs' not in bootstrap.lower()
    elif (root/'nvme').exists():
        assert 'graphroot = "/mnt/nvme/podman"' in bootstrap
        assert 'mount_program = "/usr/bin/fuse-overlayfs"' in bootstrap
        assert 'Requires=mj-ec2-cache.service' in bootstrap
    else:
        assert '/mnt/nvme/podman' not in bootstrap
    if (root/'fail-launch').exists(): sys.exit('An error occurred (InsufficientInstanceCapacity) when calling the RunInstances operation: capacity unavailable')
    tags = next(t['Tags'] for t in request['TagSpecifications'] if t['ResourceType']=='instance')
    assert next(t['Value'] for t in tags if t['Key']=='mj:operation') == request['ClientToken']
    instance = {'InstanceId':'i-test','State':{'Name':'running'},'Placement':request['Placement'],'InstanceType':request['InstanceType'],'PublicIpAddress':'192.0.2.1','Tags':tags}
    state.write_text(json.dumps(instance))
    if (root/'lost-launch-response').exists(): sys.exit('connection lost after launch; InsufficientInstanceCapacity in unrelated diagnostic')
    response = {'Instances':[instance]}
elif op == 'allocate-address':
    response = {'AllocationId':'eipalloc-test','PublicIp':'192.0.2.7','Tags':request['TagSpecifications'][0]['Tags']}
    (resources/'address.json').write_text(json.dumps(response))
elif op == 'associate-address':
    address=json.loads((resources/'address.json').read_text());address['AssociationId']='eipassoc-test';(resources/'address.json').write_text(json.dumps(address))
elif op == 'describe-addresses':
    response={'Addresses':[json.loads((resources/'address.json').read_text())] if (resources/'address.json').exists() else []}
elif op == 'disassociate-address':
    address=json.loads((resources/'address.json').read_text());address.pop('AssociationId');(resources/'address.json').write_text(json.dumps(address))
elif op == 'release-address': (resources/'address.json').unlink()
elif op == 'terminate-instances':
    if (root/'fail-terminate').exists(): sys.exit('termination refused')
    assert request['InstanceIds'] == ['i-test']
    instance = json.loads(state.read_text()); instance['State']['Name']='terminated'; state.write_text(json.dumps(instance))
elif op == 'describe-security-groups':
    assert any(f['Name']=='tag:mj:operation' for f in request['Filters'])
    response = {'SecurityGroups':[json.loads((resources/'group.json').read_text())] if (resources/'group.json').exists() else []}
elif op == 'describe-key-pairs':
    assert any(f['Name']=='tag:mj:operation' for f in request['Filters'])
    response = {'KeyPairs':[json.loads((resources/'key.json').read_text())] if (resources/'key.json').exists() else []}
elif op == 'delete-security-group': (resources/'group.json').unlink()
elif op == 'delete-key-pair': (resources/'key.json').unlink()
elif op not in ('authorize-security-group-ingress','wait instance-status-ok','wait instance-terminated'):
    sys.exit('unexpected AWS operation: '+op)
print(json.dumps(response))
"#;

    const PODMAN_FIXTURE: &str = r#"#!/usr/bin/python3
import fcntl, json, os, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
if args == ['info', '--format', '{{.Host.Security.Rootless}}']:
    print('true')
elif args[:2] == ['image', 'inspect']:
    if not (root/'image-ready').exists(): sys.exit(125)
    print('sha256:downloaded-image')
elif args == ['image', 'prune', '-f']:
    pass
elif args and args[0] == 'pull':
    config = pathlib.Path(os.environ['MJ_CONFIG_DIR'])/'config.toml'
    assert config.exists() and 'builder' in config.read_text(), 'host must be registered before preloading'
    # Hard-won: c66e5f4: a slow preload held the operation lock and blocked destroy.
    with (config.parent/'ec2'/'locks'/'builder.lock').open('a') as operation:
        fcntl.flock(operation, fcntl.LOCK_EX | fcntl.LOCK_NB)
    locks = list((pathlib.Path(os.environ['MJ_DATA_DIR'])/'image-pulls').glob('*.lock'))
    assert len(locks) == 1, 'preload must use the shared mj image gate'
    with locks[0].open('a') as gate:
        try:
            fcntl.flock(gate, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            pass
        else:
            sys.exit('preload did not hold the cross-process image gate')
    (root/'image-pull-args').write_text(json.dumps(args))
    if (root/'fail-image-pull').exists():
        sys.stderr.write('download progress\n' * 5000 + 'registry refused the image download\n')
        sys.exit(1)
    (root/'image-ready').write_text('ready')
    print('sha256:downloaded-image')
else:
    sys.exit('unexpected Podman command: '+repr(args))
"#;

    fn fixture_child(name: &str, expected_warning: Option<&str>) -> bool {
        fixture_output(name, expected_warning).is_some()
    }

    fn fixture_output(name: &str, expected_warning: Option<&str>) -> Option<Output> {
        if std::env::var_os("MJ_EC2_FIXTURE_CHILD").is_some() {
            return None;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for (name, body) in [
            ("aws", AWS_FIXTURE),
            (
                "ssh",
                concat!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$MJ_EC2_FIXTURE_CHILD/ssh-calls\"\n",
                    "for remote_command do :; done\n",
                    // The host's boot ID lives in a fixture file that a reboot replaces.
                    "[ -e \"$MJ_EC2_FIXTURE_CHILD/boot_id\" ] || echo boot-1 > \"$MJ_EC2_FIXTURE_CHILD/boot_id\"\n",
                    "remote_command=$(printf '%s' \"$remote_command\" | sed \"s#/proc/sys/kernel/random/boot_id#$MJ_EC2_FIXTURE_CHILD/boot_id#g\")\n",
                    "case \"$remote_command\" in\n",
                    "*'systemctl --no-block reboot'*) echo boot-2 > \"$MJ_EC2_FIXTURE_CHILD/boot_id\" ;;\n",
                    "*dmesg*) printf '6.18.56-126.188.amzn2023\\nnitro_enclaves: CMA pool: 49152 MiB reserved\\n' ;;\n",
                    "*) exec sh -c \"$remote_command\" ;;\nesac\n",
                ),
            ),
            ("podman", PODMAN_FIXTURE),
            ("cloud-init", "#!/bin/sh\nprintf 'status: done\\n'\n"),
        ] {
            let path = bin.join(name);
            fs::write(&path, body).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut paths = vec![bin];
        paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", name, "--nocapture"])
            .env("MJ_EC2_FIXTURE_CHILD", directory.path())
            .env("MJ_CONFIG_DIR", directory.path().join("config"))
            .env("MJ_DATA_DIR", directory.path().join("data"))
            .env("MJ_INSTANCE", "ec2-fixture")
            .env("MJ_SSH_CONTROL_MASTER", "0")
            .env("PATH", std::env::join_paths(paths).unwrap());
        let output = checked(&mut command, &[]).unwrap();
        if let Some(warning) = expected_warning {
            assert!(String::from_utf8_lossy(&output.stderr).contains(warning));
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("registry refused the image download")
            );
        }
        Some(output)
    }

    fn create_args() -> CreateArgs {
        CreateArgs {
            instance_type: vec!["c8id.16xlarge".into()],
            types: Vec::new(),
            runtime: Ec2Runtime::Podman,
            zone: vec!["us-east-1a".into()],
            region: Vec::new(),
            aws_profile: None,
            name: Some("builder".into()),
            subnet: None,
            ssh_cidr: Some("192.0.2.3/32".into()),
            disk_gib: 200,
            disk_mbps: None,
            disk_iops: None,
            zfs_cache: false,
            json: true,
        }
    }

    #[test]
    fn capacity_preferences_clean_refusals_skip_regions_and_preserve_inventory_history() {
        if let Some(output) = fixture_output(
            "ec2::tests::capacity_preferences_clean_refusals_skip_regions_and_preserve_inventory_history",
            None,
        ) {
            let stdout = String::from_utf8(output.stdout).unwrap();
            let start = stdout.find('{').expect("JSON create result");
            let host = serde_json::Deserializer::from_str(&stdout[start..])
                .into_iter::<Value>()
                .next()
                .unwrap()
                .unwrap();
            assert_eq!(host["region"], "us-west-2");
            assert_eq!(host["zone"], "us-west-2b");
            assert_eq!(host["instance_type"], "m8id.8xlarge");
            assert_eq!(host["zfs_cache"], false);
            assert_eq!(host["attempts"].as_array().unwrap().len(), 10);
            assert!(stdout.contains("VcpuLimitExceeded") && stdout.contains("OptInRequired"));
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(bin.join("multi-region"), "").unwrap();
        // Hard-won: #1523: a deleted default subnet stopped the region-only search.
        fs::write(
            bin.join("missing-subnets.json"),
            r#"["us-west-1a","us-west-1b"]"#,
        )
        .unwrap();
        fs::write(bin.join("errors.json"), serde_json::to_vec(&json!([
            {"operation":"run-instances","region":"us-east-1","code":"VcpuLimitExceeded","message":"On-Demand Standard vCPU quota exceeded"},
            {"operation":"describe-instances","region":"us-east-2","code":"OptInRequired","message":"Region is not opted in"},
            {"operation":"run-instances","region":"us-west-2","instance_type":"c8id.16xlarge","zone":"us-west-2a","code":"InsufficientInstanceCapacity","message":"No capacity; suggested zones are hints"},
            {"operation":"run-instances","region":"us-west-2","instance_type":"c8id.16xlarge","zone":"us-west-2b","code":"Unsupported","message":"The requested instance type is not supported in this availability zone"}
        ])).unwrap()).unwrap();
        fs::write(
            bin.join("unoffered.json"),
            r#"[["us-west-2","m8id.8xlarge","us-west-2a"]]"#,
        )
        .unwrap();
        let cli = crate::Cli::try_parse_from([
            "mj",
            "--instance",
            "ec2-fixture",
            "ec2",
            "create",
            "--type",
            "c8id.16xlarge,m8id.8xlarge",
            "--zone",
            "us-east-1b",
            "--zone",
            "us-east-1a",
            "--region",
            "us-east-1,us-east-2,us-west-1",
            "--region",
            "us-west-2",
            "--name",
            "builder",
            "--ssh-cidr",
            "192.0.2.3/32",
            "--json",
        ])
        .unwrap();
        let Some(crate::Command::Ec2(Ec2Args {
            command: Ec2Command::Create(args),
        })) = cli.command
        else {
            panic!("expected EC2 create command");
        };
        create(args, &root).unwrap();
        let record = records(&root).unwrap().pop().unwrap();
        assert_eq!(record.region, "us-west-2");
        assert_eq!(record.attempts.len(), 10);
        assert_eq!(record.attempts[0].outcome, "skipped_region");
        assert_eq!(record.attempts[1].outcome, "skipped_region");
        assert_eq!(record.attempts.last().unwrap().outcome, "created");
        let calls: Vec<Value> = fs::read_to_string(bin.join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let launches: Vec<_> = calls
            .iter()
            .filter(|call| call["operation"] == "run-instances")
            .collect();
        assert!(record.attempts[2..6].iter().all(|attempt| {
            attempt
                .reason
                .as_deref()
                .unwrap()
                .contains("no default subnet")
        }));
        assert!(!calls.iter().any(|call| {
            call["operation"] == "import-key-pair"
                && call["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|arg| arg == "us-west-1")
        }));
        let placements: Vec<_> = launches
            .iter()
            .map(|call| {
                (
                    call["request"]["InstanceType"].as_str().unwrap(),
                    call["request"]["Placement"]["AvailabilityZone"]
                        .as_str()
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(
            placements,
            [
                ("c8id.16xlarge", "us-east-1b"),
                ("c8id.16xlarge", "us-west-2a"),
                ("c8id.16xlarge", "us-west-2b"),
                ("m8id.8xlarge", "us-west-2b"),
            ]
        );
        let tokens: std::collections::BTreeSet<_> = launches
            .iter()
            .map(|call| call["request"]["ClientToken"].as_str().unwrap())
            .collect();
        assert_eq!(
            tokens.len(),
            launches.len(),
            "different requests must never reuse AWS client tokens"
        );
        let (rows, complete) = inventory(
            &ListArgs {
                aws: AwsArgs::default(),
                json: true,
            },
            &root,
        )
        .unwrap();
        assert!(
            complete,
            "an explicitly disabled region does not hide usable inventory"
        );
        assert!(
            rows.iter()
                .any(|row| row["region"] == "us-east-2" && row["error"].is_string())
        );
        let winner = rows
            .iter()
            .find(|row| row["instance_id"] == "i-test")
            .unwrap();
        assert_eq!(winner["region"], "us-west-2");
        assert_eq!(winner["attempts"].as_array().unwrap().len(), 10);
        list(
            ListArgs {
                aws: AwsArgs::default(),
                json: false,
            },
            &root,
        )
        .unwrap();
        destroy(
            DestroyArgs {
                host: "builder".into(),
                aws: AwsArgs::default(),
            },
            &root,
        )
        .unwrap();
        for region in ["us-east-1", "us-east-2", "us-west-1", "us-west-2"] {
            assert!(!bin.join(region).join("key.json").exists());
            assert!(!bin.join(region).join("group.json").exists());
        }
    }

    #[test]
    fn cleanup_failure_stops_capacity_fallback_and_retains_recovery_keys() {
        if fixture_child(
            "ec2::tests::cleanup_failure_stops_capacity_fallback_and_retains_recovery_keys",
            None,
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(bin.join("errors.json"), serde_json::to_vec(&json!([
            {"operation":"run-instances","code":"InsufficientInstanceCapacity","message":"No capacity"},
            {"operation":"delete-security-group","code":"UnauthorizedOperation","message":"Cleanup refused"}
        ])).unwrap()).unwrap();
        let mut args = create_args();
        args.zone.push("us-east-1b".into());
        let error = create(args, &root).unwrap_err();
        assert!(format!("{error:#}").contains("refusal cleanup failed"));
        let record = records(&root).unwrap().pop().unwrap();
        assert_eq!(record.state, RecordState::Active);
        assert!(format!("{error:#}").contains(&record.operation));
        assert!(record.directory(&root).unwrap().join("id_ed25519").exists());
        assert!(bin.join("key.json").exists() && bin.join("group.json").exists());
        let calls = fs::read_to_string(bin.join("calls")).unwrap();
        assert_eq!(
            calls
                .lines()
                .filter(
                    |line| serde_json::from_str::<Value>(line).unwrap()["operation"]
                        == "run-instances"
                )
                .count(),
            1
        );
        fs::remove_file(bin.join("errors.json")).unwrap();
        destroy(
            DestroyArgs {
                host: record.operation,
                aws: AwsArgs::default(),
            },
            &root,
        )
        .unwrap();
        assert!(records(&root).unwrap().is_empty());
    }

    #[test]
    fn uncertain_launch_and_errors_after_acceptance_never_launch_another_candidate() {
        if fixture_child(
            "ec2::tests::uncertain_launch_and_errors_after_acceptance_never_launch_another_candidate",
            None,
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        for uncertain in [true, false] {
            if uncertain {
                fs::write(bin.join("lost-launch-response"), "").unwrap();
            } else {
                fs::write(bin.join("errors.json"), r#"[{"operation":"wait instance-status-ok","code":"InsufficientInstanceCapacity","message":"An error after the launch was accepted"}]"#).unwrap();
            }
            let mut args = create_args();
            args.zone.push("us-east-1b".into());
            let error = create(args, &root).unwrap_err();
            assert!(format!("{error:#}").contains("retained for inspection"));
            let record = records(&root).unwrap().pop().unwrap();
            assert_eq!(record.instance_id.is_none(), uncertain);
            assert!(record.directory(&root).unwrap().join("id_ed25519").exists());
            let calls: Vec<Value> = fs::read_to_string(bin.join("calls"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                calls
                    .iter()
                    .filter(|call| call["operation"] == "run-instances")
                    .count(),
                if uncertain { 1 } else { 2 }
            );
            fs::remove_file(bin.join(if uncertain {
                "lost-launch-response"
            } else {
                "errors.json"
            }))
            .unwrap();
            destroy(
                DestroyArgs {
                    host: record.operation,
                    aws: AwsArgs::default(),
                },
                &root,
            )
            .unwrap();
            assert!(records(&root).unwrap().is_empty());
        }
    }

    #[test]
    fn aws_cli_contract_preserves_configuration_and_keys_until_termination_succeeds() {
        if fixture_child(
            "ec2::tests::aws_cli_contract_preserves_configuration_and_keys_until_termination_succeeds",
            None,
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(
            bin.join("ebs-settings"),
            r#"{"VolumeSize":512,"Throughput":500,"Iops":12000}"#,
        )
        .unwrap();
        let cli = crate::Cli::try_parse_from([
            "mj",
            "--instance",
            "ec2-fixture",
            "ec2",
            "create",
            "c8id.16xlarge",
            "--zone",
            "us-east-1a",
            "--name",
            "builder",
            "--ssh-cidr",
            "192.0.2.3/32",
            "--disk-gib",
            "512",
            "--disk-mbps",
            "500",
            "--disk-iops",
            "12000",
            "--json",
        ])
        .unwrap();
        let Some(crate::Command::Ec2(Ec2Args {
            command: Ec2Command::Create(args),
        })) = cli.command
        else {
            panic!("expected EC2 create command");
        };
        create(args, &root).unwrap();
        assert!(bin.join("image-ready").exists());
        let pulled: Vec<String> =
            serde_json::from_slice(&fs::read(bin.join("image-pull-args")).unwrap()).unwrap();
        assert_eq!(
            pulled,
            [
                "pull",
                "--platform=linux/amd64",
                mj_core::config::DEFAULT_CONTAINER_IMAGE
            ]
        );

        let record = records(&root).unwrap().pop().unwrap();
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("state");
        legacy.as_object_mut().unwrap().remove("attempts");
        fs::write(
            record.directory(&root).unwrap().join("resources.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        // Hard-won: #1481: a terminated VM with the same target blocked teardown.
        fs::write(bin.join("historical"), "").unwrap();
        let before = Config::load().unwrap();
        let Some(Machine::Ssh { ssh, .. }) = before.machines.get("builder") else {
            panic!("missing SSH machine");
        };
        assert_eq!(ssh.host, "192.0.2.7");
        assert!(matches!(
            before.targets.get("builder"),
            Some(TargetTemplate::SshPodman { .. })
        ));
        // A concurrent, unrelated editor change must survive destruction.
        Config::update(|config| {
            config.machines.insert(
                "other".into(),
                Machine::Ssh {
                    ssh: SshConnection {
                        host: "other.example".into(),
                        user: None,
                        identity_file: None,
                        extra_args: vec![],
                    },
                    workspace_prefix: "workspaces".into(),
                    build_cache: None,
                },
            );
            Ok(())
        })
        .unwrap();
        fs::write(bin.join("account"), "999999999999").unwrap();
        let wrong_account = destroy(
            DestroyArgs {
                host: "builder".into(),
                aws: AwsArgs::default(),
            },
            &root,
        )
        .unwrap_err();
        assert!(
            wrong_account
                .to_string()
                .contains("differs from the host's account")
        );
        assert!(record.directory(&root).unwrap().join("id_ed25519").exists());
        fs::remove_file(bin.join("account")).unwrap();
        fs::write(bin.join("fail-terminate"), "").unwrap();
        let destroy_args = || DestroyArgs {
            host: "builder".into(),
            aws: AwsArgs::default(),
        };
        assert!(
            destroy(destroy_args(), &root)
                .unwrap_err()
                .to_string()
                .contains("terminate-instances")
        );
        assert!(Config::load().unwrap().targets.contains_key("builder"));
        assert!(record.directory(&root).unwrap().join("id_ed25519").exists());
        assert!(
            bin.join("key.json").exists()
                && bin.join("group.json").exists()
                && bin.join("address.json").exists()
        );
        fs::remove_file(bin.join("fail-terminate")).unwrap();
        destroy(destroy_args(), &root).unwrap();
        let after = Config::load().unwrap();
        assert!(!after.machines.contains_key("builder") && !after.targets.contains_key("builder"));
        assert!(after.machines.contains_key("other"));
        assert!(!record.directory(&root).unwrap().exists());
        assert!(
            !bin.join("key.json").exists()
                && !bin.join("group.json").exists()
                && !bin.join("address.json").exists()
        );
        let calls: Vec<Value> = fs::read_to_string(bin.join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let terminated = calls
            .iter()
            .rposition(|c| c["operation"] == "wait instance-terminated")
            .unwrap();
        let removed = calls
            .iter()
            .position(|c| c["operation"] == "delete-security-group")
            .unwrap();
        assert!(terminated < removed);
    }

    #[test]
    fn nitro_create_uses_enclave_ami_and_verified_bootstrap_before_registration() {
        if fixture_child(
            "ec2::tests::nitro_create_uses_enclave_ami_and_verified_bootstrap_before_registration",
            None,
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(bin.join("nitro"), "").unwrap();
        let mut args = create_args();
        args.runtime = Ec2Runtime::Nitro;
        create(args, &root).unwrap();

        let calls: Vec<Value> = fs::read_to_string(bin.join("calls"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let launch = calls
            .iter()
            .find(|call| call["operation"] == "run-instances")
            .unwrap();
        assert_eq!(launch["request"]["EnclaveOptions"]["Enabled"], true);
        let bootstrap = String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(launch["request"]["UserData"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(
            bootstrap.contains("1ac8eec866dd5eea14efecf6323e22b23eea24054545338a5b34bc5bca6623cd")
        );
        assert!(bootstrap.contains("nitro_enclaves.mempool=95%-1G"));
        assert!(
            !calls
                .iter()
                .any(|call| call["operation"] == "describe-images")
        );
        let ssh_calls = fs::read_to_string(fixture.join("ssh-calls")).unwrap();
        assert!(ssh_calls.contains("ec2-user@192.0.2.1"));
        assert!(ssh_calls.contains("ec2-user@192.0.2.7"));
        let record = records(&root).unwrap().pop().unwrap();
        assert_eq!(record.instance_id.as_deref(), Some("i-test"));
        let config = Config::load().unwrap();
        assert!(matches!(
            config.targets.get("builder"),
            Some(TargetTemplate::SshNitro { .. })
        ));
        assert!(
            calls
                .iter()
                .any(|call| call["operation"] == "allocate-address")
        );
        assert!(
            calls
                .iter()
                .any(|call| call["operation"] == "associate-address")
        );
    }

    #[test]
    fn refused_launch_is_cleaned_and_history_does_not_block_another_create() {
        if fixture_child(
            "ec2::tests::refused_launch_is_cleaned_and_history_does_not_block_another_create",
            None,
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(bin.join("nvme"), "").unwrap();
        fs::write(bin.join("fail-launch"), "").unwrap();
        let error = create(create_args(), &root).unwrap_err();
        assert!(format!("{error:#}").contains("all refused attempts were cleaned up"));
        let record = records(&root).unwrap().pop().unwrap();
        assert_eq!(record.state, RecordState::CleanedUp);
        assert!(
            record.attempts[0]
                .reason
                .as_deref()
                .unwrap()
                .contains("InsufficientInstanceCapacity")
        );
        assert!(!record.directory(&root).unwrap().join("id_ed25519").exists());
        assert!(!bin.join("key.json").exists() && !bin.join("group.json").exists());
        // The history survives, but does not force a manual destroy before retry.
        create(create_args(), &root).unwrap_err();
        assert_eq!(records(&root).unwrap().len(), 1);
        let record = records(&root).unwrap().pop().unwrap();
        assert!(!Config::load().unwrap().targets.contains_key("builder"));
        destroy(
            DestroyArgs {
                host: record.operation,
                aws: AwsArgs::default(),
            },
            &root,
        )
        .unwrap();
        assert!(records(&root).unwrap().is_empty());
        assert!(
            !fs::read_to_string(bin.join("calls"))
                .unwrap()
                .contains("terminate-instances")
        );
        assert!(!bin.join("key.json").exists() && !bin.join("group.json").exists());
    }

    // Hard-won: 0703826: an optional image-pull failure stranded a ready, billable VM.
    #[test]
    fn ssh_setup_selects_existing_service_unit() {
        use std::os::unix::fs::PermissionsExt;

        let setup = include_str!("ec2_sshd.sh")
            .split("# Measured relay-session refusals")
            .next()
            .unwrap();
        for (available, succeeds) in [("ssh.service", true), ("sshd.service", true), ("", false)] {
            let directory = tempfile::tempdir().unwrap();
            let systemctl = directory.path().join("systemctl");
            fs::write(
                &systemctl,
                "#!/bin/sh\nif [ \"$1\" = cat ]; then [ \"$2\" = \"$MJ_SSH_UNIT\" ]; else echo \"$*\" >> \"$MJ_SYSTEMCTL_LOG\"; fi\n",
            )
            .unwrap();
            fs::set_permissions(&systemctl, fs::Permissions::from_mode(0o700)).unwrap();
            let output = Command::new("/bin/sh")
                .arg("-c")
                .arg(setup)
                .env("PATH", directory.path())
                .env("MJ_SSH_UNIT", available)
                .env("MJ_SYSTEMCTL_LOG", directory.path().join("calls"))
                .output()
                .unwrap();
            assert_eq!(output.status.success(), succeeds);
            if succeeds {
                assert_eq!(
                    fs::read_to_string(directory.path().join("calls")).unwrap(),
                    format!("start {available}\n")
                );
            } else {
                assert!(
                    String::from_utf8_lossy(&output.stderr)
                        .contains("Neither ssh.service nor sshd.service exists")
                );
            }
        }
    }

    #[test]
    fn zfs_setup_gives_ubuntu_its_config_directory_before_subdirectories() {
        // `install -d -o ubuntu .../.config/x` makes a missing ~/.config root-owned; the
        // user-owned ~/.config must be created first or mbx setup cannot write beside it.
        let lines: Vec<&str> = include_str!("ec2_zfs.sh").lines().map(str::trim).collect();
        let first_child = lines
            .iter()
            .position(|line| line.contains("/home/ubuntu/.config/"))
            .expect("the setup writes under /home/ubuntu/.config");
        let parent = lines
            .iter()
            .position(|line| *line == "install -d -o ubuntu -g ubuntu /home/ubuntu/.config")
            .expect("the setup creates /home/ubuntu/.config owned by ubuntu");
        assert!(
            parent < first_child,
            "~/.config must be created before {:?}",
            lines[first_child]
        );
    }

    #[test]
    fn image_download_failure_warns_and_registers_the_usable_host() {
        if fixture_child(
            "ec2::tests::image_download_failure_warns_and_registers_the_usable_host",
            Some("Warning: dev image predownload failed"),
        ) {
            return;
        }
        let fixture = PathBuf::from(std::env::var_os("MJ_EC2_FIXTURE_CHILD").unwrap());
        let bin = fixture.join("bin");
        let root = config_dir().join("ec2");
        fs::write(bin.join("fail-image-pull"), "").unwrap();
        create(create_args(), &root).unwrap();
        assert!(!bin.join("image-ready").exists());
        let record = records(&root).unwrap().pop().unwrap();
        assert_eq!(record.instance_id.as_deref(), Some("i-test"));
        assert!(record.directory(&root).unwrap().join("id_ed25519").exists());
        let config = Config::load().unwrap();
        assert!(config.machines.contains_key("builder"));
        assert!(matches!(
            config.targets.get("builder"),
            Some(TargetTemplate::SshPodman { .. })
        ));
        destroy(
            DestroyArgs {
                host: record.operation,
                aws: AwsArgs::default(),
            },
            &root,
        )
        .unwrap();
        assert!(records(&root).unwrap().is_empty());
        let instance: Value =
            serde_json::from_slice(&fs::read(bin.join("instance.json")).unwrap()).unwrap();
        assert_eq!(instance["State"]["Name"], "terminated");
    }
}
