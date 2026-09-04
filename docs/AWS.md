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

Mjolnir adds two tags as part of that launch:

- `dev.mj.session=<session-id>` identifies the owning Mjolnir session.
- `dev.mj.managed=true` lets recovery discover Mjolnir-managed instances.

The `run-instances` response supplies the exact instance ID. Mjolnir waits for
that instance to enter the `running` state, then calls `describe-instances` by
ID to read its public DNS, public IP, private DNS, or private IP according to
`address_source`. Recovery also uses `describe-instances`, filtered by the
managed tag and instance state, to find surviving session instances.

Closing the session runs the matching, idempotent:

```console
aws --profile <aws_profile> --region <region> ec2 terminate-instances --instance-ids <instance-id>
```

Beyond launching, tagging, describing, and terminating the session instance,
Mjolnir does not provision AWS infrastructure. It does not create a launch
template, security group, key pair, VPC, or IAM role, and it does not manage
AMIs. Prepare those resources before you point a target at AWS.

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

Mjolnir does not create those prerequisites. `mj setup` checks for a usable AWS
CLI identity before offering AWS setup, `mj doctor` checks the configured
identity and launch template, and session preflight checks the identity and
configured launch-template version. These checks do not replace configuring
the launch template, network, and SSH access correctly.

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

Accepted target keys:

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

For the complete normal dashboard and session lifecycle, including instance-size
selection, provisioning preflight, address resolution, recovery, and teardown,
the AWS identity needs these EC2 actions:

- `ec2:DescribeLaunchTemplateVersions`
- `ec2:DescribeInstanceTypes`
- `ec2:RunInstances`
- `ec2:CreateTags` — `RunInstances` applies the two Mjolnir tags at launch.
- `ec2:DescribeInstances` — used by the instance-running waiter, address lookup,
  and recovery scan.
- `ec2:TerminateInstances`

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

## `mj setup` and `mj doctor`

`mj setup` can offer an AWS target when a configured `aws` CLI is detected;
`mj doctor` validates the prerequisites it's able to check without launching
an instance. Neither replaces the manual steps above: creating the launch
template, security group, and key material is still on you.
