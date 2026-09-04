# AWS EC2 targets

This is the setup guide for a Mjolnir `aws-ec2` target: a disposable EC2 instance
that Mjolnir launches for one session and terminates when the session closes.

## What Mjolnir does, and does not, manage

Mjolnir provisions a session by shelling out to the `aws` CLI. Opening a session
on an `aws-ec2` target runs:

```console
aws --profile <aws_profile> --region <region> ec2 run-instances \
  --launch-template LaunchTemplateName=<launch_template>[,Version=<launch_template_version>] \
  ...
```

(`src/hel_targets.rs`, around the `TargetTemplate::AwsEc2` arm of
`provision_plan`). Closing the session runs the matching, idempotent:

```console
aws --profile <aws_profile> --region <region> ec2 terminate-instances --instance-ids <instance-id>
```

Mjolnir reads the instance's address (public DNS, public IP, private DNS, or
private IP, per `address_source`) out of the `run-instances` response itself;
it does not call `describe-instances` to locate a session's instance.

Mjolnir does not create any other AWS resource. It does not create a launch
template, security group, key pair, VPC, or IAM role, and it does not manage
AMIs. All of that is your responsibility, prepared before you point a target
at AWS.

## Prerequisites you set up by hand

- The `aws` CLI installed and on `PATH`.
- Working AWS credentials for the region you'll launch in. If you use a named
  profile, set `aws_profile` in the target (see below); otherwise Mjolnir uses
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

Mjolnir does not create or validate any of the above; it only launches instances
from the template you name and connects to them over SSH.

## Target configuration

Add an `aws-ec2` target under `[targets.<name>]` in `config.toml`. The target
name is arbitrary — it's just the label you pick under `[targets.*]` and use
wherever Mjolnir asks you to choose a target.

```toml
[targets.ec2]
kind = "aws-ec2"
region = "us-east-1"
launch_template = "mj-agent"
ssh_user = "ubuntu"
address_source = "public-dns"
# aws_profile = "work"
# launch_template_version = "3"
# identity_file = "/home/me/.ssh/mj-ec2"
# ssh_args = ["-o", "StrictHostKeyChecking=accept-new"]
```

Keys, verified against `TargetTemplate::AwsEc2` in `src/hel_config.rs`:

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
With `--write-mj-config`, it appends a target block to your `config.toml`
named `[targets.aws-runson]` — a fixed name chosen by the script, unrelated to
the `mj-runson` launch template name or to any target name you might use
elsewhere (such as `ec2` in the example above). The target name under
`[targets.*]` is always arbitrary; pick whatever you like when writing one by
hand.

## IAM permissions

Based on the actual AWS API calls in `src/hel_targets.rs` and
`scripts/update-runson-launch-template.sh`, an IAM principal used with Mjolnir
needs, at minimum:

For normal session launch/teardown (`mj` itself):

- `ec2:RunInstances`
- `ec2:TerminateInstances`

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

Grant only what you need: a principal that only runs sessions against an
existing launch template needs `RunInstances`/`TerminateInstances`; the wider
list is only needed by whoever runs the launch-template updater.

## `mj setup` and `mj doctor`

`mj setup` can offer an AWS target when a configured `aws` CLI is detected;
`mj doctor` validates the prerequisites it's able to check without launching
an instance. Neither replaces the manual steps above: creating the launch
template, security group, and key material is still on you.
