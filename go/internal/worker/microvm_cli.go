//go:build linux

// Port of mj-worker/src/microvm/cli.rs.
//
// The Podman-compatible command line of the engine.
//
// Only the verbs, flags and output formats that Mjolnir's Podman code uses
// exist. Anything else is an error that names it: nothing is ignored
// silently. Engine failures exit 125, as Podman's do.
//
// Rust parses with clap's derive; the Go build parses the same grammar
// (microvmCLIParse) and prints clap's help, version and error text.

package worker

import (
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// microvmEngineFailure is the exit status of an engine failure, as Podman
// uses.
const microvmEngineFailure = 125

// microvmCLI is the parsed command line: `mj-worker microvm`, "Run OCI
// images as Nitro Sandbox microVMs with a Podman-compatible command line",
// with `--version`.
type microvmCLI struct {
	command microvmCommand
}

// microvmCLIError is a clap error: a usage error (printed to stderr, exit
// 125), or the help or version text (printed to stdout, exit 0).
type microvmCLIError struct {
	text      string
	useStderr bool
}

func (e *microvmCLIError) Error() string { return e.text }

// print writes the text where clap writes it.
func (e *microvmCLIError) print() error {
	var out io.Writer = os.Stdout
	if e.useStderr {
		out = os.Stderr
	}
	_, err := io.WriteString(out, e.text)
	return err
}

// microvmCommand is the subcommand.
type microvmCommand interface {
	isMicrovmCommand()
}

// microvmCommandInfo reports the engine and host facts.
type microvmCommandInfo struct{ args microvmInfoArgs }

// microvmCommandPull fetches an image and flattens it into a bootable disk.
type microvmCommandPull struct{ args microvmPullArgs }

// microvmCommandImage is the image commands.
type microvmCommandImage struct{ command microvmImageCommand }

// microvmCommandRun creates and starts a VM.
type microvmCommandRun struct{ args *microvmRunArgs }

// microvmCommandExec runs a command in a running VM.
type microvmCommandExec struct{ args microvmExecArgs }

// microvmCommandCp copies files between the host and a running VM.
type microvmCommandCp struct{ args microvmCpArgs }

// microvmCommandContainer is the container (VM) commands.
type microvmCommandContainer struct{ command microvmContainerCommand }

// microvmCommandPs lists VMs.
type microvmCommandPs struct{ args microvmPsArgs }

// microvmCommandStart starts stopped VMs from their retained disks.
type microvmCommandStart struct{ args microvmNamesArgs }

// microvmCommandStop stops VMs.
type microvmCommandStop struct{ args microvmStopArgs }

// microvmCommandRm removes VMs.
type microvmCommandRm struct{ args microvmRmArgs }

// microvmCommandVolume is the volume commands.
type microvmCommandVolume struct{ command microvmVolumeCommand }

// microvmCommandGuestAgent is the guest agent. Runs as PID 1 inside a VM.
// Hidden.
type microvmCommandGuestAgent struct{}

// microvmCommandRootCheckInner, inside a private mount namespace, checks an
// image mounted at root; run by `container replace-root stage`. Hidden.
type microvmCommandRootCheckInner struct {
	root string
	path string
}

// microvmCommandHostTapDown removes a TAP device; run by the VM's systemd
// unit when it stops. Hidden.
type microvmCommandHostTapDown struct{ tap string }

// microvmCommandHostJailDown removes a stopped VM's TAP device and jail;
// run by the VM's unit when it stops. Hidden.
type microvmCommandHostJailDown struct{ slot uint32 }

func (microvmCommandInfo) isMicrovmCommand()           {}
func (microvmCommandPull) isMicrovmCommand()           {}
func (microvmCommandImage) isMicrovmCommand()          {}
func (microvmCommandRun) isMicrovmCommand()            {}
func (microvmCommandExec) isMicrovmCommand()           {}
func (microvmCommandCp) isMicrovmCommand()             {}
func (microvmCommandContainer) isMicrovmCommand()      {}
func (microvmCommandPs) isMicrovmCommand()             {}
func (microvmCommandStart) isMicrovmCommand()          {}
func (microvmCommandStop) isMicrovmCommand()           {}
func (microvmCommandRm) isMicrovmCommand()             {}
func (microvmCommandVolume) isMicrovmCommand()         {}
func (microvmCommandGuestAgent) isMicrovmCommand()     {}
func (microvmCommandRootCheckInner) isMicrovmCommand() {}
func (microvmCommandHostTapDown) isMicrovmCommand()    {}
func (microvmCommandHostJailDown) isMicrovmCommand()   {}

type microvmInfoArgs struct {
	// format: only `json` is supported.
	format *string
}

type microvmPullArgs struct {
	platform *string
	// quiet is accepted; the image id is the only output either way.
	quiet bool
	image string
}

// microvmImageCommand is the image subcommand.
type microvmImageCommand interface {
	isMicrovmImageCommand()
}

// microvmImageCommandInspect describes an image.
type microvmImageCommandInspect struct {
	format *string
	image  string
}

// microvmImageCommandPrune removes images no reference or VM uses.
type microvmImageCommandPrune struct{ force bool }

// microvmImageCommandExists exits 0 when the image is known, 1 when not.
type microvmImageCommandExists struct{ image string }

func (microvmImageCommandInspect) isMicrovmImageCommand() {}
func (microvmImageCommandPrune) isMicrovmImageCommand()   {}
func (microvmImageCommandExists) isMicrovmImageCommand()  {}

type microvmRunArgs struct {
	detach  bool
	name    string
	labels  []string
	cpus    *float64
	memory  *string
	env     []string
	volumes []string
	user    *string
	workdir *string
	pull    *string
	// platform, as for pull.
	platform *string
	// init is accepted: the guest agent is always an init.
	init bool
	// stopSignal is accepted when KILL: stopping a VM always kills it.
	stopSignal *string
	// capAdd, capDrop and securityOpt are accepted with no effect: the VM
	// is the isolation boundary.
	capAdd      []string
	capDrop     []string
	securityOpt []string
	// pidsLimit is accepted with no effect: each VM has its own kernel and
	// process table.
	pidsLimit *string
	image     string
	// command is trailing and may hold hyphenated words.
	command []string
}

type microvmExecArgs struct {
	interactive bool
	detach      bool
	user        *string
	env         []string
	workdir     *string
	container   string
	// command is trailing, required, and may hold hyphenated words.
	command []string
}

type microvmCpArgs struct {
	source      string
	destination string
}

// microvmContainerCommand is the container subcommand.
type microvmContainerCommand interface {
	isMicrovmContainerCommand()
}

// microvmContainerCommandExists exits 0 when the VM exists, 1 when not.
type microvmContainerCommandExists struct{ name string }

// microvmContainerCommandInspect describes a VM as a one-element JSON
// array.
type microvmContainerCommandInspect struct {
	format *string
	name   string
}

// microvmContainerCommandReplaceRoot replaces a stopped VM's root disk with
// a clone of another image, keeping its volumes (the controller's
// retained-storage image upgrade).
type microvmContainerCommandReplaceRoot struct{ command microvmReplaceRootCommand }

func (microvmContainerCommandExists) isMicrovmContainerCommand()      {}
func (microvmContainerCommandInspect) isMicrovmContainerCommand()     {}
func (microvmContainerCommandReplaceRoot) isMicrovmContainerCommand() {}

// microvmReplaceRootCommand is the replace-root subcommand.
type microvmReplaceRootCommand interface {
	isMicrovmReplaceRootCommand()
}

// microvmReplaceRootCommandStage records the target image and keeps it
// from `image prune`; prints its id.
type microvmReplaceRootCommandStage struct {
	// operation is the controller's operation id; every step of one
	// replacement names it.
	operation string
	// labels are the labels the swap adds to the VM.
	labels []string
	name   string
	image  string
}

// microvmReplaceRootCommandApply swaps the root disk of the stopped VM;
// prints the VM's new id.
type microvmReplaceRootCommandApply struct {
	operation string
	name      string
}

// microvmReplaceRootCommandRelease deletes the previous root disk and the
// record.
type microvmReplaceRootCommandRelease struct {
	operation string
	name      string
}

func (microvmReplaceRootCommandStage) isMicrovmReplaceRootCommand()   {}
func (microvmReplaceRootCommandApply) isMicrovmReplaceRootCommand()   {}
func (microvmReplaceRootCommandRelease) isMicrovmReplaceRootCommand() {}

type microvmPsArgs struct {
	// all includes VMs that are not running.
	all bool
	// filters are `label=KEY` or `label=KEY=VALUE`; repeated filters must
	// all match.
	filters []string
	// format is `json`, or a template applied to each VM.
	format *string
}

type microvmNamesArgs struct {
	// names is required.
	names []string
}

type microvmStopArgs struct {
	// time is the seconds the guest has to flush and power off before the
	// VM is killed.
	time   *uint64
	ignore bool
	names  []string
}

type microvmRmArgs struct {
	force  bool
	ignore bool
	// volumes also removes the VM's named volumes.
	volumes bool
	names   []string
}

// microvmVolumeCommand is the volume subcommand.
type microvmVolumeCommand interface {
	isMicrovmVolumeCommand()
}

type microvmVolumeCommandCreate struct {
	labels []string
	name   string
}

type microvmVolumeCommandRm struct {
	// force ignores volumes that do not exist.
	force bool
	names []string
}

type microvmVolumeCommandLs struct{ format *string }

type microvmVolumeCommandInspect struct {
	format *string
	name   string
}

type microvmVolumeCommandExists struct{ name string }

func (microvmVolumeCommandCreate) isMicrovmVolumeCommand()  {}
func (microvmVolumeCommandRm) isMicrovmVolumeCommand()      {}
func (microvmVolumeCommandLs) isMicrovmVolumeCommand()      {}
func (microvmVolumeCommandInspect) isMicrovmVolumeCommand() {}
func (microvmVolumeCommandExists) isMicrovmVolumeCommand()  {}

// microvmCLIParse parses args (the words after `microvm`) as clap parses
// them for the binary name `mj-worker microvm`. A usage error, `--help`
// and `--version` come back as a *microvmCLIError.
func microvmCLIParse(args []string) (*microvmCLI, error) {
	panic("unported: mj-worker/src/microvm/cli.rs Cli::try_parse_from")
}

// MicrovmCLIMain runs the engine with args (the words after `microvm`) and
// returns the exit status.
func MicrovmCLIMain(args []string) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs main")
}

func microvmCLIDispatch(layout MicrovmLayout, command microvmCommand) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs dispatch")
}

func microvmCLIReplaceRoot(layout MicrovmLayout, command microvmReplaceRootCommand) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs replace_root")
}

// microvmCLIPrintInfo prints each member of info as `key: value`.
func microvmCLIPrintInfo(info any) {
	object, ok := info.(jsonvalue.Object)
	if !ok {
		return
	}
	for _, member := range object {
		encoded, err := jsonvalue.Marshal(member.Value)
		if err != nil {
			// Every value the engine describes is plain JSON, as serde_json's
			// Value is; one that is not is a defect here.
			panic(fmt.Sprintf("microvm info member %s cannot be encoded: %v", member.Key, err))
		}
		fmt.Printf("%s: %s\n", member.Key, encoded)
	}
}

// microvmCLIPrintDescribed prints a described object as Podman does: a
// one-element array, or the `--format` rendering.
func microvmCLIPrintDescribed(description any, format *string) error {
	panic("unported: mj-worker/src/microvm/cli.rs print_described")
}

func microvmCLIImageCommand(layout MicrovmLayout, command microvmImageCommand) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs image_command")
}

func microvmCLIParseMemory(text string) (uint64, error) {
	panic("unported: mj-worker/src/microvm/cli.rs parse_memory")
}

func microvmCLIParseVolume(text string) (MicrovmVolumeArg, error) {
	panic("unported: mj-worker/src/microvm/cli.rs parse_volume")
}

func microvmCLIParseLabels(labels []string) (map[string]string, error) {
	parsed := map[string]string{}
	for _, label := range labels {
		key, value, _ := strings.Cut(label, "=")
		if err := MicrovmValidateLabel(key, value); err != nil {
			return nil, err
		}
		parsed[key] = value
	}
	return parsed, nil
}

// microvmCLIResolveEnv: `--env KEY` without a value takes the engine's own
// value, as Podman does, so a caller can pass a secret through
// `sudo --preserve-env` instead of the command line. An unset variable is
// omitted.
func microvmCLIResolveEnv(entries []string) []string {
	resolved := []string{}
	for _, entry := range entries {
		if strings.Contains(entry, "=") {
			resolved = append(resolved, entry)
		} else if value, ok := os.LookupEnv(entry); ok {
			resolved = append(resolved, entry+"="+value)
		}
	}
	return resolved
}

func microvmCLIList(layout MicrovmLayout, args microvmPsArgs) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs list")
}

func microvmCLIRun(layout MicrovmLayout, args microvmRunArgs) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs run")
}

func microvmCLIExec(layout MicrovmLayout, args microvmExecArgs) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs exec")
}

// microvmCLISplitContainerOperand splits a `cp` operand: `name:path` inside
// a VM; ok is false for anything else, a host path.
func microvmCLISplitContainerOperand(operand string) (name string, path string, ok bool) {
	if strings.HasPrefix(operand, "/") || strings.HasPrefix(operand, ".") {
		return "", "", false
	}
	return strings.Cut(operand, ":")
}

func microvmCLICopy(layout MicrovmLayout, args microvmCpArgs) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs copy")
}

func microvmCLIVolumeCommand(layout MicrovmLayout, command microvmVolumeCommand) (int, error) {
	panic("unported: mj-worker/src/microvm/cli.rs volume_command")
}

func microvmCLIDescribeVolume(layout MicrovmLayout, metadata MicrovmVolumeMetadata) jsonvalue.Object {
	return jsonvalue.Object{
		{Key: "Name", Value: metadata.Name},
		{Key: "Driver", Value: "local"},
		{Key: "Mountpoint", Value: layout.Volume(metadata.Name)},
		{Key: "CreatedAt", Value: metadata.Created},
		{Key: "Labels", Value: microvmLabelsObject(metadata.Labels)},
	}
}
