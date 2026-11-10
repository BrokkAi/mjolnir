//! Persistent AWS hosts are ordinary SSH machines running rootless Podman.
//! AWS tags own cloud inventory; the local record owns keys and recovery after
//! an interrupted create. No daemon or dashboard waits on this provisioning.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use clap::{Args, Subcommand};
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
    /// Launch Ubuntu with rootless Podman and add an SSH machine/target.
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
    /// AWS region. Create infers it from --zone; others use the AWS default
    /// or the saved provisioning record when selecting a local host.
    #[arg(long)]
    region: Option<String>,
    /// AWS CLI credential profile (independent of an mj harness profile).
    #[arg(long)]
    aws_profile: Option<String>,
}

#[derive(Debug, Args)]
struct CreateArgs {
    /// EC2 instance type, for example c8id.16xlarge.
    instance_type: String,
    /// Availability zone, for example us-east-1a.
    #[arg(long)]
    zone: String,
    /// Machine and Podman target name. Omitted generates a unique name.
    #[arg(long)]
    name: Option<String>,
    #[command(flatten)]
    aws: AwsArgs,
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
        let output = checked(&mut command, &[])
            .with_context(|| format!("aws {service} {}", operation.join(" ")))?;
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

fn create(args: CreateArgs, root: &Path) -> Result<()> {
    let mut random = [0u8; 16];
    getrandom::fill(&mut random).map_err(|e| anyhow::anyhow!("generate EC2 operation ID: {e}"))?;
    let operation = mj_core::hex::lower_hex(random);
    let target = args
        .name
        .clone()
        .unwrap_or_else(|| format!("ec2-{}", &operation[..8]));
    validate_id("machine", &target)?;
    ensure!(target != "local", "local is reserved for this computer");
    let region = match &args.aws.region {
        Some(region) => region.clone(),
        None => {
            // AWS region names end at their numeric component; this also
            // handles GovCloud regions and zones such as us-west-2-lax-1a.
            let parts: Vec<_> = args.zone.split('-').collect();
            let end = parts
                .iter()
                .position(|part| part.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .context("cannot infer region from --zone; pass --region")?;
            let number = parts[end].trim_end_matches(|c: char| c.is_ascii_alphabetic());
            let mut region = parts[..end].join("-");
            region.push('-');
            region.push_str(number);
            region
        }
    };
    let cloud = Cloud::new(AwsArgs {
        region: Some(region.clone()),
        aws_profile: args.aws.aws_profile.clone(),
    });
    fs::create_dir_all(root)?;
    // One owner serializes creates and destroys of this local target. Never
    // hold the shared config lock across cloud or network operations.
    let _lock = lock_target(root, &target)?;
    let config = Config::load()?;
    ensure!(
        !config.machines.contains_key(&target) && !config.targets.contains_key(&target),
        "machine or target {target:?} already exists"
    );
    ensure!(
        !records(root)?.iter().any(|r| r.target == target),
        "an earlier create for {target:?} remains; use mj ec2 list, then destroy its operation before retrying"
    );
    ensure!(
        !cloud
            .instances()?
            .iter()
            .any(|i| tag(i, "mj:target").as_deref() == Some(&target)
                && i["State"]["Name"] != "terminated"),
        "managed EC2 host {target:?} already exists in {region}"
    );
    let info = cloud.ec2(
        "describe-instance-types",
        json!({"InstanceTypes":[args.instance_type]}),
    )?;
    let instance_type = info["InstanceTypes"]
        .as_array()
        .and_then(|v| v.first())
        .context("AWS returned no instance type")?;
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
    let local_nvme = instance_type["InstanceStorageInfo"]["TotalSizeInGB"]
        .as_u64()
        .is_some_and(|size| size > 0)
        && instance_type["InstanceStorageInfo"]["NvmeSupport"]
            .as_str()
            .is_some_and(|support| support != "unsupported");
    if args.zfs_cache {
        ensure!(
            instance_type["InstanceStorageInfo"]["TotalSizeInGB"]
                .as_u64()
                .is_some_and(|n| n > 0),
            "{} has no local SSDs for --zfs-cache",
            args.instance_type
        );
        ensure!(
            instance_type["InstanceStorageInfo"]["NvmeSupport"]
                .as_str()
                .is_some_and(|v| v != "unsupported"),
            "--zfs-cache requires local NVMe instance storage"
        );
    }
    let subnet = if let Some(subnet) = &args.subnet {
        cloud.ec2("describe-subnets", json!({"SubnetIds":[subnet]}))?
    } else {
        cloud.ec2("describe-subnets", json!({"Filters":[{"Name":"availability-zone","Values":[args.zone]},{"Name":"default-for-az","Values":["true"]}]}))?
    };
    let subnets = subnet["Subnets"]
        .as_array()
        .context("AWS returned no Subnets")?;
    ensure!(
        subnets.len() == 1,
        "no unique default subnet in {}; pass --subnet",
        args.zone
    );
    ensure!(
        subnets[0]["AvailabilityZone"] == args.zone,
        "--subnet is outside --zone"
    );
    let subnet_id = subnets[0]["SubnetId"]
        .as_str()
        .context("subnet has no ID")?;
    let vpc = subnets[0]["VpcId"].as_str().context("subnet has no VPC")?;
    let parameter = cloud.call("ssm", &["get-parameter"], json!({"Name":format!("/aws/service/canonical/ubuntu/server/24.04/stable/current/{ami_arch}/hvm/ebs-gp3/ami-id")}))?;
    let ami = parameter["Parameter"]["Value"]
        .as_str()
        .context("Ubuntu SSM parameter has no image ID")?;
    let image = cloud.ec2(
        "describe-images",
        json!({"ImageIds":[ami],"Owners":["099720109477"]}),
    )?;
    let image = image["Images"]
        .as_array()
        .and_then(|v| v.first())
        .context("image is not published by Canonical")?;
    let root_device = image["RootDeviceName"]
        .as_str()
        .context("Ubuntu image has no root device")?;
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
    let mut record = Record {
        account,
        operation,
        target,
        region,
        aws_profile: args.aws.aws_profile.clone(),
        instance_id: None,
    };
    let directory = record.directory(root)?;
    fs::create_dir(&directory)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    }
    record.save(root)?;
    eprintln!(
        "Creating {}. Recovery/cleanup: {}",
        record.target,
        cleanup_command(&record)
    );
    let result = (|| -> Result<Value> {
        let key = directory.join("id_ed25519");
        checked(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-f"])
                .arg(&key),
            &[],
        )?;
        let key_name = format!("mj-ec2-{}", record.operation);
        cloud.ec2("import-key-pair", json!({"KeyName":key_name,"PublicKeyMaterial":base64::engine::general_purpose::STANDARD.encode(fs::read(directory.join("id_ed25519.pub"))?),"TagSpecifications":[tags(&record,"key-pair")]}))?;
        let group = cloud.ec2("create-security-group", json!({"GroupName":key_name,"Description":format!("Mjolnir Podman host {}",record.target),"VpcId":vpc,"TagSpecifications":[tags(&record,"security-group")]}))?;
        let group_id = group["GroupId"]
            .as_str()
            .context("AWS returned no security group ID")?;
        cloud.ec2("authorize-security-group-ingress", json!({"GroupId":group_id,"IpPermissions":[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"IpRanges":[{"CidrIp":cidr}]}]}))?;
        let bootstrap = bootstrap(local_nvme);
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
        let launched = cloud.ec2("run-instances", json!({
            "ImageId":ami,"InstanceType":args.instance_type,"MinCount":1,"MaxCount":1,
            "KeyName":key_name,"ClientToken":record.operation,"Placement":{"AvailabilityZone":args.zone},
            "NetworkInterfaces":[{"DeviceIndex":0,"SubnetId":subnet_id,"Groups":[group_id],"AssociatePublicIpAddress":true,"DeleteOnTermination":true}],
            "BlockDeviceMappings":[{"DeviceName":root_device,"Ebs":ebs}],
            "MetadataOptions":{"HttpTokens":"required","HttpEndpoint":"enabled"},
            "UserData":base64::engine::general_purpose::STANDARD.encode(bootstrap),
            "TagSpecifications":[tags(&record,"instance"),tags(&record,"volume")]
        }))?;
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
        let ssh = connection(&record, root, host)?;
        mj_controller::controller::wait_for_ec2_ssh(
            &mj_controller::targets::ProcessExecutor,
            &(&ssh).into(),
        )?;
        let output = checked(
            &mut ssh_command(
                &ssh,
                "cloud-init status --wait; status=$?; if [ \"$status\" -ne 0 ]; then sudo tail -100 /var/log/cloud-init-output.log; exit \"$status\"; fi; podman info --format '{{.Host.Security.Rootless}}'",
            ),
            &[],
        )?;
        ensure!(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .any(|line| line.trim() == "true"),
            "Podman did not report rootless operation"
        );
        // Attach the persistent address after cloud-init's package downloads
        // finish, before publishing a host that can accept session launches.
        let address = cloud.ec2(
            "allocate-address",
            json!({
                "Domain":"vpc", "TagSpecifications":[tags(&record,"elastic-ip")]
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
        let ssh = connection(&record, root, host)?;
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
        if local_nvme {
            eprintln!("Installing mj's pinned mbx on the new host.");
            mj_controller::controller::install_mbx(
                &machine,
                &mj_controller::targets::ProcessExecutor,
            )?;
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
            config.targets.insert(
                record.target.clone(),
                TargetTemplate::SshPodman {
                    ssh: ssh.clone(),
                    container: ContainerTemplate {
                        image: image.into(),
                        pull_policy: Default::default(),
                        platform: Some(platform.into()),
                        cpus: None,
                        memory: None,
                        environment: Default::default(),
                        workspace_storage: Default::default(),
                        build_cache: None,
                    },
                },
            );
            // A configured disabled kind suppresses runtime discovery, so a
            // Podman-only host never acquires a second, usable bare target.
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
        eprintln!(
            "Registered Podman target {}. Preloading {image}.",
            record.target
        );
        let preload = mj_core::targets::image_refresh(
            mj_core::targets::ImageHost::SshPodman((&ssh).into()),
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
            json!({"instance_id":id,"target":record.target,"region":record.region,"zone":args.zone,"instance_type":args.instance_type,"public_ip":host,"zfs_cache":args.zfs_cache,"operation":record.operation}),
        )
    })();
    let host = result.with_context(|| {
        format!(
            "provisioning resources were retained for inspection; {}",
            cleanup_command(&record)
        )
    })?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&host)?);
    } else {
        println!(
            "Created {} ({}) at {} in {}.\nPodman target: {}\n{}",
            record.target,
            host["instance_id"].as_str().unwrap_or_default(),
            host["public_ip"].as_str().unwrap_or_default(),
            args.zone,
            record.target,
            cleanup_command(&record)
        );
    }
    Ok(())
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
    let directory = record.directory(root)?;
    Ok(SshConnection {
        host: host.into(),
        user: Some("ubuntu".into()),
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

fn list(args: ListArgs, root: &Path) -> Result<()> {
    let saved = records(root)?;
    let mut regions = std::collections::BTreeSet::new();
    if let Some(region) = &args.aws.region {
        regions.insert(Some(region.clone()));
    } else {
        for record in &saved {
            if record.aws_profile == args.aws.aws_profile {
                regions.insert(Some(record.region.clone()));
            }
        }
        if regions.is_empty() {
            regions.insert(None);
        }
    }
    let mut rows = Vec::new();
    for region in regions {
        let cloud = Cloud::new(AwsArgs {
            region: region.clone(),
            ..args.aws.clone()
        });
        for instance in cloud.instances()? {
            rows.push(json!({"instance_id":instance["InstanceId"],"target":tag(&instance,"mj:target"),"operation":tag(&instance,"mj:operation"),"state":instance["State"]["Name"],"instance_type":instance["InstanceType"],"zone":instance["Placement"]["AvailabilityZone"],"public_ip":instance["PublicIpAddress"],"region":region}));
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
        if !rows.iter().any(|row| row["operation"] == record.operation) {
            rows.push(json!({"instance_id":record.instance_id,"target":record.target,"operation":record.operation,"state":"incomplete or no longer in AWS inventory","region":record.region}));
        }
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&rows)?);
    } else {
        println!("TARGET\tINSTANCE\tTYPE\tZONE\tSTATE\tPUBLIC IP\tOPERATION");
        for row in rows {
            println!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}",
                text_field(&row, "target"),
                text_field(&row, "instance_id"),
                text_field(&row, "instance_type"),
                text_field(&row, "zone"),
                text_field(&row, "state"),
                text_field(&row, "public_ip"),
                text_field(&row, "operation")
            );
        }
    }
    Ok(())
}

fn text_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("-")
}

fn destroy(args: DestroyArgs, root: &Path) -> Result<()> {
    let saved: Vec<_> = records(root)?
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
        }
    } else {
        bail!(
            "no mj-managed host or local provisioning operation matches {:?}",
            args.host
        )
    };
    let directory = record.directory(root)?;
    let _lock = lock_target(root, &record.target)?;
    let record = if directory.join("resources.json").is_file() {
        serde_json::from_slice::<Record>(&fs::read(directory.join("resources.json"))?)?
    } else {
        record
    };
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
    remove_configuration(&record, root, &config_path())?;
    if directory.exists() {
        fs::remove_dir_all(&directory)
            .with_context(|| format!("remove local EC2 keys {}", directory.display()))?;
    }
    println!("Destroyed {}.", record.target);
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
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y podman uidmap slirp4netns fuse-overlayfs dbus-user-session git curl ca-certificates mold {packages}
loginctl enable-linger ubuntu
systemctl start user@1000.service
{zfs}
sudo -u ubuntu XDG_RUNTIME_DIR=/run/user/1000 podman info --format '{{{{.Host.Security.Rootless}}}}'
"#,
        packages = if zfs_cache { "zfsutils-linux" } else { "" }
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
assert args[args.index('--region')+1] == 'us-east-1'
assert args[args.index('--cli-binary-format')+1] == 'base64'
response = {}
state = root/'instance.json'
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
elif op == 'describe-instance-types':
    kind = {'ProcessorInfo':{'SupportedArchitectures':['x86_64']}}
    if (root/'nvme').exists(): kind['InstanceStorageInfo'] = {'TotalSizeInGB':3800,'NvmeSupport':'required'}
    response = {'InstanceTypes':[kind]}
elif op == 'describe-subnets':
    response = {'Subnets':[{'SubnetId':'subnet-test','VpcId':'vpc-test','AvailabilityZone':'us-east-1a'}]}
elif op == 'get-parameter': response = {'Parameter':{'Value':'ami-test'}}
elif op == 'describe-images':
    assert request['Owners'] == ['099720109477']
    response = {'Images':[{'RootDeviceName':'/dev/sda1'}]}
elif op == 'import-key-pair':
    import base64
    assert base64.b64decode(request['PublicKeyMaterial']).startswith(b'ssh-ed25519 ')
    (root/'key.json').write_text(json.dumps({'KeyPairId':'key-test','Tags':request['TagSpecifications'][0]['Tags']}))
elif op == 'create-security-group':
    (root/'group.json').write_text(json.dumps({'GroupId':'sg-test','Tags':request['TagSpecifications'][0]['Tags']}))
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
    if (root/'nvme').exists():
        assert 'graphroot = "/mnt/nvme/podman"' in bootstrap
        assert 'mount_program = "/usr/bin/fuse-overlayfs"' in bootstrap
        assert 'Requires=mj-ec2-cache.service' in bootstrap
    else:
        assert '/mnt/nvme/podman' not in bootstrap
    if (root/'fail-launch').exists(): sys.exit('InsufficientInstanceCapacity')
    tags = next(t['Tags'] for t in request['TagSpecifications'] if t['ResourceType']=='instance')
    assert next(t['Value'] for t in tags if t['Key']=='mj:operation') == request['ClientToken']
    instance = {'InstanceId':'i-test','State':{'Name':'running'},'Placement':request['Placement'],'InstanceType':request['InstanceType'],'PublicIpAddress':'192.0.2.1','Tags':tags}
    state.write_text(json.dumps(instance))
    response = {'Instances':[instance]}
elif op == 'allocate-address':
    response = {'AllocationId':'eipalloc-test','PublicIp':'192.0.2.7','Tags':request['TagSpecifications'][0]['Tags']}
    (root/'address.json').write_text(json.dumps(response))
elif op == 'associate-address':
    address=json.loads((root/'address.json').read_text());address['AssociationId']='eipassoc-test';(root/'address.json').write_text(json.dumps(address))
elif op == 'describe-addresses':
    response={'Addresses':[json.loads((root/'address.json').read_text())] if (root/'address.json').exists() else []}
elif op == 'disassociate-address':
    address=json.loads((root/'address.json').read_text());address.pop('AssociationId');(root/'address.json').write_text(json.dumps(address))
elif op == 'release-address': (root/'address.json').unlink()
elif op == 'terminate-instances':
    if (root/'fail-terminate').exists(): sys.exit('termination refused')
    assert request['InstanceIds'] == ['i-test']
    instance = json.loads(state.read_text()); instance['State']['Name']='terminated'; state.write_text(json.dumps(instance))
elif op == 'describe-security-groups':
    assert any(f['Name']=='tag:mj:operation' for f in request['Filters'])
    response = {'SecurityGroups':[json.loads((root/'group.json').read_text())] if (root/'group.json').exists() else []}
elif op == 'describe-key-pairs':
    assert any(f['Name']=='tag:mj:operation' for f in request['Filters'])
    response = {'KeyPairs':[json.loads((root/'key.json').read_text())] if (root/'key.json').exists() else []}
elif op == 'delete-security-group': (root/'group.json').unlink()
elif op == 'delete-key-pair': (root/'key.json').unlink()
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
        if std::env::var_os("MJ_EC2_FIXTURE_CHILD").is_some() {
            return false;
        }
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for (name, body) in [
            ("aws", AWS_FIXTURE),
            (
                "ssh",
                "#!/bin/sh\nfor remote_command do :; done\nexec sh -c \"$remote_command\"\n",
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
        true
    }

    fn create_args() -> CreateArgs {
        CreateArgs {
            instance_type: "c8id.16xlarge".into(),
            zone: "us-east-1a".into(),
            name: Some("builder".into()),
            aws: AwsArgs::default(),
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
    fn refused_launch_remains_discoverable_and_can_be_cleaned_without_an_instance() {
        if fixture_child(
            "ec2::tests::refused_launch_remains_discoverable_and_can_be_cleaned_without_an_instance",
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
        assert!(format!("{error:#}").contains("InsufficientInstanceCapacity"));
        let record = records(&root).unwrap().pop().unwrap();
        assert!(format!("{error:#}").contains(&record.operation));
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
