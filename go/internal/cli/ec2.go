// Port of mj-cli/src/ec2.rs: the arguments of `mj ec2`. Creating, listing,
// destroying and tuning EC2 hosts is not ported yet.

package cli

import (
	"strings"

	"github.com/spf13/cobra"
)

// ec2Args are the arguments of `mj ec2`.
type ec2Args struct {
	command ec2Command
}

// ec2Command is a `mj ec2` subcommand.
type ec2Command interface{ isEc2Command() }

// ec2AwsArgs are the AWS options `list` and `destroy` share.
type ec2AwsArgs struct {
	// region is the AWS region. Omitted uses the saved host's region or the
	// AWS default.
	region *string
	// awsProfile is the AWS CLI credential profile (independent of an mj
	// harness profile).
	awsProfile *string
}

// bind declares the flattened AWS options on meta.
func (args *ec2AwsArgs) bind(meta *clapCommand) {
	meta.option("region", "AWS region. Omitted uses the saved host's region or the AWS default", storeOptional(&args.region))
	meta.option("aws_profile", "AWS CLI credential profile (independent of an mj harness profile)", storeOptional(&args.awsProfile))
}

// ec2Runtime is the host runtime `mj ec2 create` installs.
type ec2Runtime string

const (
	ec2RuntimePodman ec2Runtime = "podman"
	ec2RuntimeNitro  ec2Runtime = "nitro"
)

// ec2CommandCreate launches a host (Ubuntu with rootless Podman, or Amazon
// Linux with Nitro Sandboxes) and adds an SSH machine/target. Charges
// continue until you stop or destroy the VM.
type ec2CommandCreate struct {
	// instanceType is the instance types in preference order
	// (comma-separated). Also accepts --type. Required unless types is
	// given; conflicts with it.
	instanceType []string
	// types is the instance types in preference order; repeat or separate
	// with commas (`--type`).
	types []string
	// runtime is the host runtime to install. Nitro hosts run workloads in
	// isolated microVMs.
	runtime ec2Runtime
	// cpus is the default CPU count for each session container (at least 1).
	cpus *uint32
	// memoryGiB is the default memory in GiB for each session container (at
	// least 1).
	memoryGiB *uint32
	// mbxRemote is the read-only S3 build cache for Nitro sandboxes, as
	// s3://bucket/prefix.
	mbxRemote *string
	// mbxRemoteRegion is the region of the S3 bucket. Defaults to the Nitro
	// host's region.
	mbxRemoteRegion *string
	// zone is the zones in preference order; repeat or separate with commas.
	// Listed regions without explicit zones try all available ordinary
	// zones. Required unless region is given.
	zone []string
	// region is the regions in preference order; repeat or separate with
	// commas. Omitted infers regions from --zone. Tries regions, then types,
	// then zones.
	region []string
	// awsProfile is the AWS CLI credential profile (independent of an mj
	// harness profile).
	awsProfile *string
	// name is the machine and target name. Omitted generates a unique name.
	name *string
	// subnet is a public subnet in the selected zone. Omitted uses its
	// default subnet.
	subnet *string
	// sshCIDR is the allowed SSH source CIDR. Omitted discovers this
	// controller's public IPv4.
	sshCIDR *string
	// diskGiB is the encrypted gp3 data disk size in GiB (Podman adds a 16
	// GiB OS root disk); separate from local SSDs. Default 200.
	diskGiB *uint32
	// diskMbps is the EBS gp3 throughput in MiB/s for the `--disk-gib`
	// volume. Omitted uses the AWS default.
	diskMbps *uint32
	// diskIops is the EBS gp3 provisioned IOPS for the `--disk-gib` volume.
	// Omitted uses the AWS default.
	diskIops *uint32
	// zfsCache requires local NVMe storage (used automatically when
	// available). ZFS/lz4 holds Podman and mbx; stop/start clears this
	// storage.
	zfsCache bool
	// json prints the created host as JSON. Progress still goes to stderr.
	json bool
}

// ec2CommandList lists mj-managed VMs and unfinished local provisioning
// operations.
type ec2CommandList struct {
	aws ec2AwsArgs
	// json prints the inventory as JSON.
	json bool
}

// ec2CommandDestroy terminates a managed VM and deletes its disks, sessions,
// keys, and target. It also cleans up an interrupted create; it accepts its
// operation ID.
type ec2CommandDestroy struct {
	// host is the managed target name, EC2 instance ID, or interrupted
	// create operation ID.
	host string
	aws  ec2AwsArgs
}

// ec2CommandTune re-applies mj's host tuning (kernel limits) to an existing
// managed host, so hosts made before a tuning change do not need to be
// recreated.
type ec2CommandTune struct {
	// host is the machine name from the create that made the host.
	host string
}

func (ec2CommandCreate) isEc2Command()  {}
func (ec2CommandList) isEc2Command()    {}
func (ec2CommandDestroy) isEc2Command() {}
func (ec2CommandTune) isEc2Command()    {}

// appendDelimited is clap's `value_delimiter = ','`: each value given is
// split at commas into several.
func appendDelimited(target *[]string) func(string) error {
	return func(raw string) error {
		*target = append(*target, strings.Split(raw, ",")...)
		return nil
	}
}

// bind declares the ec2 subcommands on cmd; run runs the parsed command.
func (args *ec2Args) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)

	var create ec2CommandCreate
	create.runtime = ec2RuntimePodman
	createCmd := subcommand("create", "Launch a host (Ubuntu with rootless Podman, or Amazon Linux with Nitro Sandboxes) and add an SSH machine/target. Charges continue until you stop or destroy the VM")
	meta.subcommand(createCmd)
	createClap := clapOf(createCmd)
	createClap.positional("instance_type", "Instance types in preference order (comma-separated). Also accepts --type", appendDelimited(&create.instanceType),
		multiple(), requiredUnlessPresent("types"), conflictsWith("types"))
	createClap.option("types", "Instance types in preference order; repeat or separate with commas", appendDelimited(&create.types),
		withLong("type"), multiple(), requiredUnlessPresent("instance_type"))
	createClap.option("runtime", "Host runtime to install. Nitro hosts run workloads in isolated microVMs", func(raw string) error {
		create.runtime = ec2Runtime(raw)
		return nil
	}, possibleValues(string(ec2RuntimePodman), string(ec2RuntimeNitro)), defaultValue(string(ec2RuntimePodman)))
	createClap.option("cpus", "Default CPU count for each session container", storeU32(&create.cpus, 1), required())
	createClap.option("memory_gib", "Default memory in GiB for each session container", storeU32(&create.memoryGiB, 1), required())
	createClap.option("mbx_remote", "Read-only S3 build cache for Nitro sandboxes, as s3://bucket/prefix", storeOptional(&create.mbxRemote))
	createClap.option("mbx_remote_region", "Region of the S3 bucket. Defaults to the Nitro host's region", storeOptional(&create.mbxRemoteRegion))
	createClap.option("zone", "Zones in preference order; repeat or separate with commas. Listed regions without explicit zones try all available ordinary zones", appendDelimited(&create.zone),
		multiple(), requiredUnlessPresent("region"))
	createClap.option("region", "Regions in preference order; repeat or separate with commas. Omitted infers regions from --zone. Tries regions, then types, then zones", appendDelimited(&create.region),
		multiple())
	createClap.option("aws_profile", "AWS CLI credential profile (independent of an mj harness profile)", storeOptional(&create.awsProfile))
	createClap.option("name", "Machine and target name. Omitted generates a unique name", storeOptional(&create.name))
	createClap.option("subnet", "Public subnet in the selected zone. Omitted uses its default subnet", storeOptional(&create.subnet))
	createClap.option("ssh_cidr", "Allowed SSH source CIDR. Omitted discovers this controller's public IPv4", storeOptional(&create.sshCIDR))
	createClap.option("disk_gib", "Encrypted gp3 data disk size in GiB (Podman adds a 16 GiB OS root disk); separate from local SSDs", storeU32(&create.diskGiB, 0), defaultValue("200"))
	createClap.option("disk_mbps", "EBS gp3 throughput in MiB/s for the `--disk-gib` volume. Omitted uses the AWS default", storeU32(&create.diskMbps, 0))
	createClap.option("disk_iops", "EBS gp3 provisioned IOPS for the `--disk-gib` volume. Omitted uses the AWS default", storeU32(&create.diskIops, 0))
	createClap.flag(&create.zfsCache, "zfs_cache", "Require local NVMe storage (used automatically when available). ZFS/lz4 holds Podman and mbx; stop/start clears this storage")
	createClap.flag(&create.json, "json", "Print the created host as JSON. Progress still goes to stderr")
	leaf(createCmd, func() error {
		args.command = create
		return run()
	})

	var list ec2CommandList
	listCmd := subcommand("list", "List mj-managed VMs and unfinished local provisioning operations")
	meta.subcommand(listCmd)
	listClap := clapOf(listCmd)
	list.aws.bind(listClap)
	listClap.flag(&list.json, "json", "Print the inventory as JSON")
	leaf(listCmd, func() error {
		args.command = list
		return run()
	})

	var destroy ec2CommandDestroy
	destroyCmd := subcommand("destroy", "Terminate a managed VM and delete its disks, sessions, keys, and target. Also cleans up an interrupted create; accepts its operation ID")
	meta.subcommand(destroyCmd)
	destroyClap := clapOf(destroyCmd)
	destroyClap.positional("host", "Managed target name, EC2 instance ID, or interrupted create operation ID", storeString(&destroy.host), required())
	destroy.aws.bind(destroyClap)
	leaf(destroyCmd, func() error {
		args.command = destroy
		return run()
	})

	var tune ec2CommandTune
	tuneCmd := subcommand("tune", "Re-apply mj's host tuning (kernel limits) to an existing managed host, so hosts made before a tuning change do not need to be recreated")
	meta.subcommand(tuneCmd)
	clapOf(tuneCmd).positional("host", "Machine name from the create that made the host", storeString(&tune.host), required())
	leaf(tuneCmd, func() error {
		args.command = tune
		return run()
	})
	group(cmd, true, nil)
}
