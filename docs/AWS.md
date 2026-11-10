# AWS EC2 machines

Mjolnir supports persistent EC2 hosts running Podman, and disposable EC2
instances launched separately for each bare session.

## Persistent Podman hosts

Use the AWS CLI credentials already configured on your controller, plus OpenSSH:

```console
mj ec2 create c8id.16xlarge --zone us-east-1a --name builder --zfs-cache
mj ec2 list
mj new --workspace work --profile codex --target builder --bundle project
mj ec2 destroy builder
```

`create` launches the current official Ubuntu 24.04 image for the instance's
CPU architecture, installs rootless Podman, waits for EC2 status checks and
cloud-init, then adds an SSH machine and a Podman target to your configuration.
Raw SSH sessions are disabled for this host; SSH is still used to manage Podman.
The VM stays running across session suspension and destruction. Its on-demand
compute, storage, and network charges continue until you stop or destroy it.

The instance type is required. `--zone` is required and supplies the region;
`--region` can override region inference. `--aws-profile` selects AWS credentials,
independently of mj's harness profiles. The default subnet must have an internet
gateway; use `--subnet` for another public subnet in that zone. The command
creates a tagged Elastic IP (stable across stop/start), a dedicated SSH key,
and a security group allowing SSH only from the
controller's discovered public IPv4, or the explicit `--ssh-cidr`. It requires
IMDSv2 and uses an encrypted 200 GiB gp3 root disk; `--disk-gib` changes its size.
The image supplies the root filesystem (ext4); it is separate from local SSDs.

`--zfs-cache` requires an instance type with local NVMe SSDs, such as C8id or
M8id. It creates a striped ZFS pool using only devices identified as EC2 instance
storage, mounts it at `/mnt/nvme`, and places mbx at `/mnt/nvme/mbx`. The pool
uses lz4 compression, 128 KiB records, `xattr=sa`, and relative access times.
The host gets mj's pinned mbx and mold, and its mbx configuration selects
`path:/usr/bin/mold`. The standard agent image also contains mold; containers
share the host's cache and configuration through mj's normal build-cache setup.
Session files and Podman storage stay on EBS. Local SSD contents survive a
reboot but are lost on stop/start, hibernation, or termination; a boot service
recreates an empty cache after stop/start. It imports an existing cache pool
and refuses to format disks carrying other signatures.

`list` shows tagged hosts and unfinished local creates. With no `--region`, it
queries regions in your saved provisioning records for the selected AWS profile,
or the AWS CLI default when there are no records. Pass `--region` to discover
hosts created from another controller. Add `--json` for machine-readable output
on `create` and `list`.

Every create prints an operation ID and cleanup command before allocating cloud
resources. If bootstrap, target registration, or a cloud request fails, the
owned resources and SSH key remain available for inspection. Use `list`, then
`destroy <operation-id>` to clean up before retrying. A lost launch response is
recoverable through the same operation's AWS tags; do not manually retry an
uncertain launch with a fresh client token.

`destroy` accepts a target name, instance ID, or operation ID. It permanently
terminates the selected managed VM, including all sessions and its root disk,
then releases its tagged Elastic IP and removes only its tagged security
group/key pair, matching mj configuration,
and local key files. It preserves credentials and configuration until cloud
termination succeeds, and can be repeated after partial cleanup. It does not
terminate VMs without the `mj:managed-by=mj-ec2` ownership tag. Any session
records on a destroyed host retain their transcript but can no longer reach
that host; suspend or destroy sessions first when you want their recovery data
or normal session cleanup preserved.

AWS permissions include EC2 instance/image/subnet/type discovery, tagged key
pair and security group creation/deletion, ingress authorization, launching,
status checks, termination, Elastic IP allocation/association/release, and SSM `GetParameter` for the public Ubuntu image.
No IAM role or VPC is created.

## Disposable instances per session

The rest of this guide describes an `aws-ec2` machine: a disposable EC2 instance
that Mjolnir launches for one bare session and terminates when the session is
suspended or destroyed.

## Launch-template ownership

Mjolnir provisions a session by shelling out to the `aws` CLI. Opening a session
on an `aws-ec2` machine runs:

```console
aws --profile <aws_profile> --region <region> ec2 run-instances \
  --launch-template LaunchTemplateName=<launch_template>[,Version=<launch_template_version>] \
  --metadata-options HttpTokens=required \
  ...
```

`--metadata-options HttpTokens=required` makes the instance accept only IMDSv2.
The instance metadata service (IMDS) at `169.254.169.254` hands out the instance
profile's credentials to code running on the instance. IMDSv2 requires a session
token obtained with an HTTP `PUT` first, which blocks simple request-forgery
reads that IMDSv1 allows. Mjolnir sets this on every instance it launches,
including Move destinations, whatever the launch template says. Before it uses
any instance it adopts (just launched, a Move destination recorded by an older
Mjolnir version, or one found again after a restart) it reads the instance's
`MetadataOptions.HttpTokens`. An instance that allows IMDSv1 is changed in place
with `aws ec2 modify-instance-metadata-options --http-tokens required` and read
back. EC2 reports a new setting as `pending` before it is in effect, so Mjolnir
adopts the instance only once `MetadataOptions.State` is `applied`, waiting with
growing pauses within the same five-minute limit it allows a new instance to
become reachable over SSH. If the change fails, does not apply in that time, or
the operation is cancelled, the launch fails with the reason and the instance is
removed. Code in a session on an EC2 machine can therefore read the instance
profile's credentials only with IMDSv2 tokens, on instances this version of
Mjolnir launches or adopts. An instance launched before keeps whatever
`HttpTokens` setting it was created with until it is replaced; Mjolnir does not
check or change instances it is not adopting. Scope the instance profile's
role to what sessions need; Mjolnir does not create or limit it.

Mjolnir adds three tags as part of that launch:

- `dev.mj.session=<session-id>` identifies the owning Mjolnir session.
- `dev.mj.managed=true` lets recovery discover Mjolnir-managed instances.
- `dev.mj.instance=<controller-identity>` associates the resource with the
  Mjolnir instance that created it.

The `run-instances` response supplies the exact instance ID. Mjolnir waits for
that instance to enter the `running` state, then calls `describe-instances` by
ID to read its public DNS, public IP, private DNS, or private IP according to
`address_source`. Recovery also uses `describe-instances`, filtered by the
managed tag and instance state, to find surviving session instances. By default,
`mj recover scan` only lists resources owned by the current Mjolnir instance;
`--all-instances` includes other instances and older, unstamped resources.

Suspending or destroying the session runs the matching, idempotent:

```console
aws --profile <aws_profile> --region <region> ec2 terminate-instances --instance-ids <instance-id>
```

For the disposable launch-template workflow, beyond launching, tagging,
describing, and terminating the session instance, Mjolnir does not provision
AWS infrastructure. It does not create a launch
template, security group, key pair, VPC, or IAM role, and it does not manage
AMIs. Prepare those resources before you point a target at AWS.

## Prerequisites you set up by hand

- The `aws` CLI installed and on `PATH`.
- Working AWS credentials for the region you'll launch in. If you use a named
  profile, set `aws_profile` in the machine (see below); otherwise Mjolnir uses
  your default profile.
- An EC2 **launch template** you create ahead of time, specifying at least an
  AMI and a security group that allows inbound SSH from wherever Mjolnir's
  controller runs. If the template embeds a key pair, or if you prefer to
  authenticate with a plain SSH key, make sure the matching private key exists
  on the Mjolnir host and matches `identity_file` below.
- A user account on the launched instance that Mjolnir's SSH user (`ssh_user`) can
  reach non-interactively over key-based SSH as soon as the instance boots
  (typically via `cloud-init`/user data baked into the AMI or launch
  template).

Mjolnir does not create those prerequisites. Add the AWS machine and target
in Settings. `mj doctor` checks the configured identity and launch template
(and warns when the template version allows IMDSv1, although Mjolnir requires
IMDSv2 on its own launches regardless; fix the template for anyone else who
launches from it), and session preflight checks the identity and
configured launch-template version. These checks do not replace configuring
the launch template, network, and SSH access correctly.

## Target configuration

Describe the launch template as a machine under `[machines.<name>]`, then add
a bare runtime on it under `[targets.<name>]`. Both names are arbitrary labels
you pick; the runtime name is the one Mjolnir asks you to choose when starting
a session.

```toml
[machines.ec2]
kind = "aws-ec2"
region = "us-east-1"
launch_template = "mj-agent"
ssh_user = "ubuntu"
address_source = "public-dns"
# aws_profile = "work"
# launch_template_version = "3"
# identity_file = "/home/me/.ssh/mj-ec2"
# ssh_args = ["-o", "StrictHostKeyChecking=accept-new"]

[targets.ec2]
kind = "bare"
machine = "ec2"
```

An EC2 machine runs a bare harness only; it accepts no container runtime.

Accepted machine keys:

| Key | Required | Notes |
| --- | --- | --- |
| `region` | yes | AWS region, e.g. `us-east-1`. |
| `launch_template` | yes | Launch template name or `lt-...` ID. |
| `ssh_user` | yes | SSH login user on the launched instance. |
| `launch_template_version` | no | Defaults to the template's default version. |
| `aws_profile` | no | Named `aws` CLI profile; omit to use the default profile. |
| `address_source` | no | One of `public-dns` (default), `public-ip`, `private-dns`, `private-ip`. |
| `identity_file` | no | Path to the SSH private key, if not using `ssh-agent` or an SSH config default. |
| `ssh_args` | no | Extra arguments appended to every `ssh` invocation for this target. |

## The RunsOn launch-template updater

`scripts/update-runson-launch-template.sh` is a maintenance script for one
specific launch template: it copies the newest RunsOn Ubuntu 26 AMI into your
AWS account and publishes that copy as the default version of an EC2 launch
template (named `mj-runson` by default), because RunsOn deregisters its
upstream AMIs over time.

This script lives in the source tree; it is **not** shipped by `install.sh`,
so it's only available if you built Mjolnir from a source checkout.

By default it reads an SSH public/private key pair at `~/.ssh/vastai.pub` /
`~/.ssh/vastai` (override with `--ssh-public-key` / `--ssh-identity-file`).
With `--write-mj-config`, it appends a machine block to your `config.toml`
named `[machines.aws-runson]` with a matching `[targets.aws-runson]` runtime —
a fixed name chosen by the script, unrelated to
the `mj-runson` launch template name or to any target name you might use
elsewhere (such as `ec2` in the example above). The target name under
`[machines.*]` and `[targets.*]` is always arbitrary; pick whatever you like when writing one by
hand.

## IAM permissions

For the complete normal dashboard and session lifecycle, including instance-size
selection, provisioning preflight, address resolution, recovery, and teardown,
the AWS identity needs these EC2 actions:

- `ec2:DescribeLaunchTemplateVersions`
- `ec2:DescribeInstanceTypes`
- `ec2:RunInstances`
- `ec2:CreateTags` — `RunInstances` applies the three Mjolnir tags at launch.
- `ec2:DescribeInstances` — used by the instance-running waiter, address lookup,
  and recovery scan.
- `ec2:TerminateInstances`
- `ec2:ModifyInstanceMetadataOptions` — used only when an instance allows
  IMDSv1 despite the launch request.

`mj doctor` additionally calls `ec2:DescribeLaunchTemplates`. Setup, doctor,
and session preflight also call `sts:GetCallerIdentity`; AWS does not require an
explicit allow for that STS operation.

This action list is not a paste-ready least-privilege policy. The
`ec2:RunInstances` statement must authorize the launch template and the EC2
resources it references or creates, such as the AMI, subnet, security group,
key pair, network interface, volume, and instance. If the launch template
attaches an IAM instance profile, the caller also needs `iam:PassRole` for that
role. Other template choices, such as customer-managed KMS keys, can add their
own permissions.

For the optional `update-runson-launch-template.sh` maintenance script:

- `ec2:DescribeImages`
- `ec2:CopyImage`
- `ec2:CreateTags`
- `ec2:DescribeVpcs`
- `ec2:DescribeSecurityGroups`
- `ec2:CreateSecurityGroup`
- `ec2:AuthorizeSecurityGroupIngress`
- `ec2:DescribeLaunchTemplates`
- `ec2:DescribeLaunchTemplateVersions`
- `ec2:CreateLaunchTemplate`
- `ec2:CreateLaunchTemplateVersion`
- `ec2:ModifyLaunchTemplate`

Grant only what you need. The updater's wider write permissions belong only to
the identity that runs that maintenance script; they are not part of ordinary
Mjolnir session operation.

## Settings and `mj doctor`

Configure the AWS machine and target in Settings. `mj setup` discovers local
agent accounts and repositories; it does not configure AWS. `mj doctor`
validates prerequisites without launching an instance. Creating the launch
template, security group, and key material still requires the steps above.
