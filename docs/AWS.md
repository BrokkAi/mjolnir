# AWS EC2 machines

Mjolnir provisions persistent EC2 hosts with Podman. Sessions share the host
and build cache; closing a session removes its container. The host remains
until `mj ec2 destroy` terminates it.

## Persistent Podman hosts

Use the AWS CLI credentials already configured on your controller, plus OpenSSH:

```console
mj ec2 create c8id.16xlarge --zone us-east-1a --name builder
mj ec2 list
mj new --workspace work --profile codex --target builder --bundle project
mj ec2 destroy builder
```

`create` launches the current official Ubuntu 24.04 image for the instance's
CPU architecture, installs rootless Podman, and waits for EC2 status checks and
cloud-init. It adds the SSH machine and Podman target to your live configuration,
then preloads mj's dev image through the same image refresher used by the daemon.
A session launched while that preload is running waits on mj's shared image gate
instead of starting another download. If preloading fails, creation keeps the
host registered and prints a warning; the daemon can retry the download.
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
IMDSv2 and uses an encrypted 200 GiB gp3 root disk. `--disk-gib` sets its size,
`--disk-mbps` sets throughput in MiB/s, and `--disk-iops` sets provisioned IOPS.
Omitted performance options use AWS's gp3 defaults (125 MiB/s and 3,000 IOPS).
AWS validates the requested size/performance combination against its
[gp3 limits](https://docs.aws.amazon.com/ebs/latest/userguide/general-purpose.html).
For example:

```console
mj ec2 create c8id.16xlarge --zone us-east-1a --name builder \
  --disk-gib 512 --disk-mbps 500 --disk-iops 12000
```

The image supplies the root filesystem (ext4); it is separate from local SSDs.

Instances with local NVMe SSDs, such as C8id or M8id, automatically use those
SSDs for a striped ZFS pool. Only devices identified as EC2 instance storage
enter the pool; EBS remains the OS disk. `--zfs-cache` additionally refuses an
instance type without suitable local NVMe storage.

The pool mounts at `/mnt/nvme` with lz4 compression, 128 KiB records,
`xattr=sa`, and relative access times. Podman's writable layers and named
volumes live at `/mnt/nvme/podman`, using rootless OverlayFS through
`fuse-overlayfs`. Each mj container has its own Podman volume mounted at `/tmp`,
so both its workspace layer and temporary files use NVMe. mbx lives at
`/mnt/nvme/mbx`; the host gets mj's pinned mbx and mold and selects
`path:/usr/bin/mold`. The standard agent image includes mold too.

Local SSD contents survive a reboot but are lost on stop/start, hibernation,
or termination, including all container workspaces and `/tmp` volumes. Suspend
sessions to verified checkpoints before stopping the host when their work must
survive. A boot service imports the existing pool or recreates empty storage
after instance-store loss. Rootless user services wait for it; disks with
unrelated signatures are never formatted.

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
