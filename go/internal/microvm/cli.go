//go:build linux

// Port of mj-microvm/src/cli.rs.
//
// The Podman-compatible command line of the engine.
//
// Only the verbs, flags and output formats that Mjolnir's Podman code uses
// exist. Anything else is an error that names it: nothing is ignored
// silently. Engine failures exit 125, as Podman's do.
//
// Rust parses with clap's derive; the Go build parses the same grammar
// (microvmCLIParse, over the command table microvmCLISpec) and prints
// clap's help, version and error text (microvm_cli_clap.go).

package microvm

import (
	"errors"
	"fmt"
	"io"
	"math/bits"
	"os"
	"sort"
	"strings"
	"time"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/core"
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

// microvmCLISpec is the command table clap's derive builds from Rust's Cli:
// every command, argument and doc comment, in declaration order.
func microvmCLISpec() *microvmClapCommand {
	flag := func(id string, short byte, long string, help string) *microvmClapArg {
		return &microvmClapArg{id: id, short: short, long: long, help: help, action: microvmClapSetTrue}
	}
	option := func(id string, short byte, long string, help string) *microvmClapArg {
		return &microvmClapArg{id: id, short: short, long: long, value: strings.ToUpper(id), help: help, action: microvmClapSet}
	}
	repeated := func(id string, short byte, long string, help string) *microvmClapArg {
		return &microvmClapArg{id: id, short: short, long: long, value: strings.ToUpper(id), help: help, action: microvmClapAppend}
	}
	positional := func(id string, help string) *microvmClapArg {
		return &microvmClapArg{id: id, value: strings.ToUpper(id), help: help, positional: true, required: true, action: microvmClapSet}
	}
	names := func() *microvmClapArg {
		return &microvmClapArg{id: "names", value: "NAMES", positional: true, required: true, multiple: true, action: microvmClapAppend}
	}
	trailing := func(required bool) *microvmClapArg {
		return &microvmClapArg{id: "command", value: "COMMAND", positional: true, required: required, multiple: true, trailing: true, action: microvmClapAppend}
	}
	required := func(arg *microvmClapArg) *microvmClapArg {
		arg.required = true
		return arg
	}
	parsed := func(arg *microvmClapArg, parse func(string) (any, error)) *microvmClapArg {
		arg.parse = parse
		return arg
	}
	command := func(name, about string, args []*microvmClapArg, subcommands ...*microvmClapCommand) *microvmClapCommand {
		return &microvmClapCommand{name: name, about: about, args: args, subcommands: subcommands}
	}
	hidden := func(command *microvmClapCommand) *microvmClapCommand {
		command.hidden = true
		return command
	}
	format := func(help string) *microvmClapArg { return option("format", 0, "format", help) }
	operation := func(help string) *microvmClapArg { return required(option("operation", 0, "operation", help)) }
	root := command(
		"mj-worker microvm",
		"Run OCI images as Nitro Sandbox microVMs with a Podman-compatible command line",
		nil,
		command("info", "Report the engine and host facts", []*microvmClapArg{format("Only `json` is supported")}),
		command("pull", "Fetch an image and flatten it into a bootable disk", []*microvmClapArg{
			option("platform", 0, "platform", ""),
			flag("quiet", 'q', "quiet", "Accepted; the image id is the only output either way"),
			positional("image", ""),
		}),
		command("image", "Image commands", nil,
			command("inspect", "Describe an image", []*microvmClapArg{format(""), positional("image", "")}),
			command("prune", "Remove images no reference or VM uses", []*microvmClapArg{flag("force", 'f', "force", "")}),
			command("exists", "Exit 0 when the image is known, 1 when not", []*microvmClapArg{positional("image", "")}),
		),
		command("run", "Create and start a VM", []*microvmClapArg{
			flag("detach", 'd', "detach", ""),
			required(option("name", 0, "name", "")),
			repeated("labels", 'l', "label", ""),
			parsed(option("cpus", 0, "cpus", ""), microvmClapParseF64),
			option("memory", 'm', "memory", ""),
			repeated("env", 'e', "env", ""),
			repeated("volumes", 'v', "volume", ""),
			option("user", 'u', "user", ""),
			option("workdir", 'w', "workdir", ""),
			option("pull", 0, "pull", ""),
			option("platform", 0, "platform", ""),
			flag("init", 0, "init", "Accepted: the guest agent is always an init"),
			option("stop_signal", 0, "stop-signal", "Accepted when KILL: stopping a VM always kills it"),
			repeated("cap_add", 0, "cap-add", "Accepted with no effect: the VM is the isolation boundary"),
			repeated("cap_drop", 0, "cap-drop", ""),
			repeated("security_opt", 0, "security-opt", ""),
			option("pids_limit", 0, "pids-limit", "Accepted with no effect: each VM has its own kernel and process table"),
			positional("image", ""),
			trailing(false),
		}),
		command("exec", "Run a command in a running VM", []*microvmClapArg{
			flag("interactive", 'i', "interactive", ""),
			flag("detach", 'd', "detach", ""),
			option("user", 'u', "user", ""),
			repeated("env", 'e', "env", ""),
			option("workdir", 'w', "workdir", ""),
			positional("container", ""),
			trailing(true),
		}),
		command("cp", "Copy files between the host and a running VM", []*microvmClapArg{
			positional("source", ""),
			positional("destination", ""),
		}),
		command("container", "Container (VM) commands", nil,
			command("exists", "Exit 0 when the VM exists, 1 when not", []*microvmClapArg{positional("name", "")}),
			command("inspect", "Describe a VM as a one-element JSON array", []*microvmClapArg{format(""), positional("name", "")}),
			command("replace-root", "Replace a stopped VM's root disk with a clone of another image, keeping its volumes (the controller's retained-storage image upgrade)", nil,
				command("stage", "Record the target image and keep it from `image prune`; prints its id", []*microvmClapArg{
					operation("The controller's operation id; every step of one replacement names it"),
					repeated("labels", 0, "label", "Labels the swap adds to the VM"),
					positional("name", ""),
					positional("image", ""),
				}),
				command("apply", "Swap the root disk of the stopped VM; prints the VM's new id", []*microvmClapArg{
					operation(""),
					positional("name", ""),
				}),
				command("release", "Delete the previous root disk and the record", []*microvmClapArg{
					operation(""),
					positional("name", ""),
				}),
			),
		),
		command("ps", "List VMs", []*microvmClapArg{
			flag("all", 'a', "all", "Include VMs that are not running"),
			repeated("filters", 0, "filter", "`label=KEY` or `label=KEY=VALUE`; repeated filters must all match"),
			format("`json`, or a template applied to each VM"),
		}),
		command("start", "Start stopped VMs from their retained disks", []*microvmClapArg{names()}),
		command("stop", "Stop VMs", []*microvmClapArg{
			parsed(option("time", 't', "time", "Seconds the guest has to flush and power off before the VM is killed"), microvmClapParseU64),
			flag("ignore", 0, "ignore", ""),
			names(),
		}),
		command("rm", "Remove VMs", []*microvmClapArg{
			flag("force", 'f', "force", ""),
			flag("ignore", 0, "ignore", ""),
			flag("volumes", 'v', "volumes", "Also remove the VM's named volumes"),
			names(),
		}),
		command("volume", "Volume commands", nil,
			command("create", "", []*microvmClapArg{repeated("labels", 'l', "label", ""), positional("name", "")}),
			command("rm", "", []*microvmClapArg{flag("force", 'f', "force", "Ignore volumes that do not exist"), names()}),
			command("ls", "", []*microvmClapArg{format("")}),
			command("inspect", "", []*microvmClapArg{format(""), positional("name", "")}),
			command("exists", "", []*microvmClapArg{positional("name", "")}),
		),
		hidden(command("guest-agent", "The guest agent. Runs as PID 1 inside a VM", nil)),
		hidden(command("root-check-inner", "Inside a private mount namespace: check an image mounted at ROOT; run by `container replace-root stage`", []*microvmClapArg{
			positional("root", ""),
			positional("path", ""),
		})),
		hidden(command("host-tap-down", "Remove a TAP device; run by the VM's systemd unit when it stops", []*microvmClapArg{positional("tap", "")})),
		hidden(command("host-jail-down", "Remove a stopped VM's TAP device and jail; run by the VM's unit when it stops", []*microvmClapArg{
			parsed(positional("slot", ""), microvmClapParseU32),
		})),
	)
	root.version = "mj-worker microvm " + core.PackageVersion + "\n"
	root.build(root.name)
	return root
}

// microvmCLIParse parses args (the words after `microvm`) as clap parses
// them for the binary name `mj-worker microvm`. A usage error, `--help`
// and `--version` come back as a *microvmCLIError.
func microvmCLIParse(args []string) (*microvmCLI, error) {
	matches, err := microvmCLISpec().parse(args)
	if err != nil {
		return nil, err
	}
	command, err := microvmCLICommandFrom(matches.sub.name, matches.sub.matches)
	if err != nil {
		return nil, err
	}
	return &microvmCLI{command: command}, nil
}

// microvmCLICommandFrom builds a subcommand from its matches, as clap's
// derive does from the ArgMatches.
func microvmCLICommandFrom(name string, m *microvmClapMatches) (microvmCommand, error) {
	switch name {
	case "info":
		return microvmCommandInfo{args: microvmInfoArgs{format: m.one("format")}}, nil
	case "pull":
		return microvmCommandPull{args: microvmPullArgs{platform: m.one("platform"), quiet: m.flag("quiet"), image: *m.one("image")}}, nil
	case "image":
		sub := m.sub.matches
		switch m.sub.name {
		case "inspect":
			return microvmCommandImage{command: microvmImageCommandInspect{format: sub.one("format"), image: *sub.one("image")}}, nil
		case "prune":
			return microvmCommandImage{command: microvmImageCommandPrune{force: sub.flag("force")}}, nil
		default:
			return microvmCommandImage{command: microvmImageCommandExists{image: *sub.one("image")}}, nil
		}
	case "run":
		args := &microvmRunArgs{
			detach:      m.flag("detach"),
			name:        *m.one("name"),
			labels:      m.many("labels"),
			memory:      m.one("memory"),
			env:         m.many("env"),
			volumes:     m.many("volumes"),
			user:        m.one("user"),
			workdir:     m.one("workdir"),
			pull:        m.one("pull"),
			platform:    m.one("platform"),
			init:        m.flag("init"),
			stopSignal:  m.one("stop_signal"),
			capAdd:      m.many("cap_add"),
			capDrop:     m.many("cap_drop"),
			securityOpt: m.many("security_opt"),
			pidsLimit:   m.one("pids_limit"),
			image:       *m.one("image"),
			command:     m.many("command"),
		}
		if cpus, ok := m.parsed["cpus"]; ok {
			value := cpus.(float64)
			args.cpus = &value
		}
		return microvmCommandRun{args: args}, nil
	case "exec":
		return microvmCommandExec{args: microvmExecArgs{
			interactive: m.flag("interactive"),
			detach:      m.flag("detach"),
			user:        m.one("user"),
			env:         m.many("env"),
			workdir:     m.one("workdir"),
			container:   *m.one("container"),
			command:     m.many("command"),
		}}, nil
	case "cp":
		return microvmCommandCp{args: microvmCpArgs{source: *m.one("source"), destination: *m.one("destination")}}, nil
	case "container":
		sub := m.sub.matches
		switch m.sub.name {
		case "exists":
			return microvmCommandContainer{command: microvmContainerCommandExists{name: *sub.one("name")}}, nil
		case "inspect":
			return microvmCommandContainer{command: microvmContainerCommandInspect{format: sub.one("format"), name: *sub.one("name")}}, nil
		}
		step := sub.sub.matches
		var replace microvmReplaceRootCommand
		switch sub.sub.name {
		case "stage":
			replace = microvmReplaceRootCommandStage{operation: *step.one("operation"), labels: step.many("labels"), name: *step.one("name"), image: *step.one("image")}
		case "apply":
			replace = microvmReplaceRootCommandApply{operation: *step.one("operation"), name: *step.one("name")}
		default:
			replace = microvmReplaceRootCommandRelease{operation: *step.one("operation"), name: *step.one("name")}
		}
		return microvmCommandContainer{command: microvmContainerCommandReplaceRoot{command: replace}}, nil
	case "ps":
		return microvmCommandPs{args: microvmPsArgs{all: m.flag("all"), filters: m.many("filters"), format: m.one("format")}}, nil
	case "start":
		return microvmCommandStart{args: microvmNamesArgs{names: m.many("names")}}, nil
	case "stop":
		args := microvmStopArgs{ignore: m.flag("ignore"), names: m.many("names")}
		if value, ok := m.parsed["time"]; ok {
			seconds := value.(uint64)
			args.time = &seconds
		}
		return microvmCommandStop{args: args}, nil
	case "rm":
		return microvmCommandRm{args: microvmRmArgs{force: m.flag("force"), ignore: m.flag("ignore"), volumes: m.flag("volumes"), names: m.many("names")}}, nil
	case "volume":
		sub := m.sub.matches
		switch m.sub.name {
		case "create":
			return microvmCommandVolume{command: microvmVolumeCommandCreate{labels: sub.many("labels"), name: *sub.one("name")}}, nil
		case "rm":
			return microvmCommandVolume{command: microvmVolumeCommandRm{force: sub.flag("force"), names: sub.many("names")}}, nil
		case "ls":
			return microvmCommandVolume{command: microvmVolumeCommandLs{format: sub.one("format")}}, nil
		case "inspect":
			return microvmCommandVolume{command: microvmVolumeCommandInspect{format: sub.one("format"), name: *sub.one("name")}}, nil
		default:
			return microvmCommandVolume{command: microvmVolumeCommandExists{name: *sub.one("name")}}, nil
		}
	case "guest-agent":
		return microvmCommandGuestAgent{}, nil
	case "root-check-inner":
		return microvmCommandRootCheckInner{root: *m.one("root"), path: *m.one("path")}, nil
	case "host-tap-down":
		return microvmCommandHostTapDown{tap: *m.one("tap")}, nil
	case "host-jail-down":
		return microvmCommandHostJailDown{slot: m.parsed["slot"].(uint32)}, nil
	}
	return nil, fmt.Errorf("unknown microvm command %s", name)
}

// MicrovmCLIMain runs the engine with args (the words after `microvm`) and
// returns the exit status.
func MicrovmCLIMain(args []string) (int, error) {
	cli, err := microvmCLIParse(args)
	if err != nil {
		var clapErr *microvmCLIError
		if !errors.As(err, &clapErr) {
			return 0, err
		}
		status := 0
		if clapErr.useStderr {
			status = microvmEngineFailure
		}
		// clap writes its own text, including --help and --version.
		if err := clapErr.print(); err != nil {
			return 0, fmt.Errorf("print command-line error: %w", err)
		}
		return status, nil
	}
	return microvmCLIDispatch(MicrovmSystemLayout(), cli.command)
}

func microvmCLIDispatch(layout MicrovmLayout, command microvmCommand) (int, error) {
	switch command := command.(type) {
	case microvmCommandGuestAgent:
		if err := MicrovmRunGuestAgent(); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmCommandHostJailDown:
		if err := MicrovmReleaseHostResources(layout, command.slot); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmCommandRootCheckInner:
		if err := MicrovmCheckMounted(command.root, command.path); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmCommandHostTapDown:
		if err := MicrovmTapDown(command.tap); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmCommandInfo:
		info, err := MicrovmVMInfo(layout)
		if err != nil {
			return 0, err
		}
		switch {
		case command.args.format == nil:
			microvmCLIPrintInfo(info)
		case *command.args.format == "json":
			text, err := microvmPrettyJSON(info)
			if err != nil {
				return 0, err
			}
			fmt.Println(text)
		default:
			return 0, fmt.Errorf("unsupported --format %s for info: use json", microvmDebugString(*command.args.format))
		}
		return 0, nil
	case microvmCommandPull:
		id, err := MicrovmImagePull(layout, command.args.image, command.args.platform)
		if err != nil {
			return 0, err
		}
		fmt.Println(id)
		return 0, nil
	case microvmCommandImage:
		return microvmCLIImageCommand(layout, command.command)
	case microvmCommandRun:
		return microvmCLIRun(layout, *command.args)
	case microvmCommandExec:
		return microvmCLIExec(layout, command.args)
	case microvmCommandCp:
		return microvmCLICopy(layout, command.args)
	case microvmCommandContainer:
		switch container := command.command.(type) {
		case microvmContainerCommandExists:
			exists, err := MicrovmVMExists(layout, container.name)
			if err != nil {
				return 0, err
			}
			if exists {
				return 0, nil
			}
			return 1, nil
		case microvmContainerCommandInspect:
			description, err := MicrovmVMInspect(layout, container.name)
			if err != nil {
				return 0, err
			}
			if err := microvmCLIPrintDescribed(description, container.format); err != nil {
				return 0, err
			}
			return 0, nil
		case microvmContainerCommandReplaceRoot:
			return microvmCLIReplaceRoot(layout, container.command)
		}
	case microvmCommandPs:
		return microvmCLIList(layout, command.args)
	case microvmCommandStart:
		for _, name := range command.args.names {
			if err := MicrovmVMStart(layout, name); err != nil {
				return 0, err
			}
			fmt.Println(name)
		}
		return 0, nil
	case microvmCommandStop:
		// `--time 0` still gets the default grace: the controller stops
		// sessions with `--time 0` and keeps their volumes, and killing the
		// VM outright would discard what the guest has not yet written. The
		// grace only bounds an unresponsive guest; a flush takes well under
		// a second.
		seconds := MicrovmDefaultStopSeconds
		if command.args.time != nil && *command.args.time != 0 {
			seconds = *command.args.time
		}
		grace := time.Duration(seconds) * time.Second
		if hi, lo := bits.Mul64(seconds, uint64(time.Second)); hi != 0 || lo > 1<<63-1 {
			// Rust's Duration holds any count of seconds; Go's tops out
			// near 292 years, which bounds the flush just as well.
			grace = time.Duration(1<<63 - 1)
		}
		for _, name := range command.args.names {
			if err := MicrovmVMStop(layout, name, grace, command.args.ignore); err != nil {
				return 0, err
			}
			fmt.Println(name)
		}
		return 0, nil
	case microvmCommandRm:
		for _, name := range command.args.names {
			if err := MicrovmVMRemove(layout, name, command.args.force, command.args.ignore, command.args.volumes); err != nil {
				return 0, err
			}
			fmt.Println(name)
		}
		return 0, nil
	case microvmCommandVolume:
		return microvmCLIVolumeCommand(layout, command.command)
	}
	return 0, fmt.Errorf("unknown microvm command %T", command)
}

func microvmCLIReplaceRoot(layout MicrovmLayout, command microvmReplaceRootCommand) (int, error) {
	if err := layout.Ensure(); err != nil {
		return 0, err
	}
	switch command := command.(type) {
	case microvmReplaceRootCommandStage:
		labels, err := microvmCLIParseLabels(command.labels)
		if err != nil {
			return 0, err
		}
		name, err := MicrovmVMResolve(layout, command.name)
		if err != nil {
			return 0, err
		}
		id, err := MicrovmReplaceRootStage(layout, name, command.operation, command.image, labels)
		if err != nil {
			return 0, err
		}
		fmt.Println(id)
	case microvmReplaceRootCommandApply:
		name, err := MicrovmVMResolve(layout, command.name)
		if err != nil {
			return 0, err
		}
		id, err := MicrovmReplaceRootApply(layout, name, command.operation)
		if err != nil {
			return 0, err
		}
		fmt.Println(id)
	case microvmReplaceRootCommandRelease:
		name, err := MicrovmVMResolve(layout, command.name)
		if err != nil {
			return 0, err
		}
		if err := MicrovmReplaceRootRelease(layout, name, command.operation); err != nil {
			return 0, err
		}
	}
	return 0, nil
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
	if format == nil {
		text, err := microvmPrettyJSON([]any{description})
		if err != nil {
			return err
		}
		fmt.Println(text)
		return nil
	}
	rendered, err := MicrovmTemplateRender(*format, description)
	if err != nil {
		return err
	}
	fmt.Print(rendered)
	return nil
}

func microvmCLIImageCommand(layout MicrovmLayout, command microvmImageCommand) (int, error) {
	switch command := command.(type) {
	case microvmImageCommandInspect:
		id, ok, err := MicrovmImageLookup(layout, command.image)
		if err != nil {
			return 0, err
		}
		if !ok {
			return 0, fmt.Errorf("%s: image not known", command.image)
		}
		config, err := MicrovmImageConfigRead(layout, id)
		if err != nil {
			return 0, err
		}
		refs, err := MicrovmImageRefsRead(layout)
		if err != nil {
			return 0, err
		}
		var tags []string
		for reference, target := range refs.Refs {
			if target == id {
				tags = append(tags, reference)
			}
		}
		sort.Strings(tags)
		description := jsonvalue.Object{
			{Key: "Id", Value: id},
			{Key: "RepoTags", Value: microvmStringValues(tags)},
			{Key: "Architecture", Value: config.Architecture},
			{Key: "Size", Value: config.RootBytes},
			{Key: "Config", Value: jsonvalue.Object{
				{Key: "User", Value: config.User},
				{Key: "Env", Value: microvmStringValues(config.Env)},
				{Key: "WorkingDir", Value: config.WorkingDir},
				{Key: "Entrypoint", Value: microvmStringValues(config.Entrypoint)},
				{Key: "Cmd", Value: microvmStringValues(config.Cmd)},
				{Key: "Labels", Value: microvmLabelsObject(config.Labels)},
			}},
		}
		if err := microvmCLIPrintDescribed(description, command.format); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmImageCommandPrune:
		if !command.force {
			return 0, errors.New("image prune needs -f: there is no confirmation prompt")
		}
		removed, err := MicrovmImagePrune(layout)
		if err != nil {
			return 0, err
		}
		for _, id := range removed {
			fmt.Println(id)
		}
		return 0, nil
	case microvmImageCommandExists:
		_, ok, err := MicrovmImageLookup(layout, command.image)
		if err != nil {
			return 0, err
		}
		if ok {
			return 0, nil
		}
		return 1, nil
	}
	return 0, fmt.Errorf("unknown image command %T", command)
}

func microvmCLIParseMemory(text string) (uint64, error) {
	lower := []byte(text)
	for i, b := range lower {
		if b >= 'A' && b <= 'Z' {
			lower[i] = b + ('a' - 'A')
		}
	}
	value := strings.TrimRight(string(lower), "b")
	digits, factor := value, uint64(1)
	switch {
	case strings.HasSuffix(value, "k"):
		digits, factor = value[:len(value)-1], 1<<10
	case strings.HasSuffix(value, "m"):
		digits, factor = value[:len(value)-1], 1<<20
	case strings.HasSuffix(value, "g"):
		digits, factor = value[:len(value)-1], 1<<30
	}
	amount, err := microvmParseUint(digits, 64)
	if err != nil {
		return 0, fmt.Errorf("invalid --memory %s: use a number with an optional b, k, m or g suffix: %w", microvmDebugString(text), err)
	}
	hi, product := bits.Mul64(amount, factor)
	if hi != 0 {
		return 0, fmt.Errorf("--memory %s is too large", microvmDebugString(text))
	}
	return product, nil
}

func microvmCLIParseVolume(text string) (MicrovmVolumeArg, error) {
	parts := strings.SplitN(text, ":", 3)
	name := parts[0]
	if len(parts) < 2 {
		return MicrovmVolumeArg{}, fmt.Errorf("invalid --volume %s: use name:/path[:options]", microvmDebugString(text))
	}
	destination := parts[1]
	if strings.HasPrefix(name, "/") || strings.HasPrefix(name, ".") || strings.HasPrefix(name, "~") {
		return MicrovmVolumeArg{}, fmt.Errorf("--volume %s names a host path; Nitro Sandboxes support named volumes only", microvmDebugString(text))
	}
	readWrite, chown := true, false
	if len(parts) == 3 {
		for _, option := range strings.Split(parts[2], ",") {
			switch option {
			case "":
			case "rw":
				readWrite = true
			case "ro":
				readWrite = false
			case "U":
				chown = true
			default:
				return MicrovmVolumeArg{}, fmt.Errorf("unsupported volume option %s in --volume %s; supported: rw, ro, U", microvmDebugString(option), microvmDebugString(text))
			}
		}
	}
	if !readWrite && chown {
		return MicrovmVolumeArg{}, fmt.Errorf("--volume %s: option U changes ownership, which a read-only volume cannot take", microvmDebugString(text))
	}
	return MicrovmVolumeArg{Name: name, Destination: destination, ReadWrite: readWrite, Chown: chown}, nil
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
	type wantedLabel struct {
		key      string
		value    string
		hasValue bool
	}
	var wanted []wantedLabel
	for _, filter := range args.filters {
		label, ok := strings.CutPrefix(filter, "label=")
		if !ok {
			return 0, fmt.Errorf("unsupported --filter %s: use label=KEY or label=KEY=VALUE", microvmDebugString(filter))
		}
		key, value, hasValue := strings.Cut(label, "=")
		wanted = append(wanted, wantedLabel{key, value, hasValue})
	}
	if err := layout.Ensure(); err != nil {
		return 0, err
	}
	states, err := MicrovmVmStateList(layout)
	if err != nil {
		return 0, err
	}
	var items []jsonvalue.Object
	for _, state := range states {
		activity, err := MicrovmVMActivity(state.Name)
		if err != nil {
			return 0, err
		}
		running := activity == MicrovmActivityRunning
		matches := true
		for _, label := range wanted {
			actual, ok := state.Labels[label.key]
			if !ok || (label.hasValue && actual != label.value) {
				matches = false
			}
		}
		if !matches || !(args.all || running) {
			continue
		}
		status := "exited"
		if running {
			status = "running"
		}
		mounts := make([]string, 0, len(state.Volumes))
		for _, volume := range state.Volumes {
			mounts = append(mounts, volume.Destination)
		}
		items = append(items, jsonvalue.Object{
			{Key: "Id", Value: state.ID},
			{Key: "Names", Value: []any{state.Name}},
			{Key: "Image", Value: state.ImageReference},
			{Key: "ImageID", Value: state.ImageID},
			{Key: "Labels", Value: microvmLabelsObject(state.Labels)},
			{Key: "State", Value: status},
			{Key: "Created", Value: state.Created},
			{Key: "Mounts", Value: microvmStringValues(mounts)},
		})
	}
	switch {
	case args.format != nil && *args.format == "json":
		list := make([]any, len(items))
		for i, item := range items {
			list[i] = item
		}
		text, err := microvmPrettyJSON(list)
		if err != nil {
			return 0, err
		}
		fmt.Println(text)
	case args.format != nil:
		// Go templates print a single name for `{{.Names}}`; give each item
		// a scalar view.
		for _, item := range items {
			scalar := item.Clone()
			names, _ := microvmObjectGet(item, "Names").([]any)
			scalar.Set("Names", names[0])
			rendered, err := MicrovmTemplateRender(*args.format, scalar)
			if err != nil {
				return 0, err
			}
			fmt.Print(rendered)
		}
	default:
		for _, item := range items {
			id, _ := microvmObjectGet(item, "Id").(string)
			status, _ := microvmObjectGet(item, "State").(string)
			names, _ := microvmObjectGet(item, "Names").([]any)
			name, _ := names[0].(string)
			fmt.Printf("%s  %s  %s\n", id[:min(12, len(id))], status, name)
		}
	}
	return 0, nil
}

func microvmCLIRun(layout MicrovmLayout, args microvmRunArgs) (int, error) {
	if !args.detach {
		return 0, errors.New("run without --detach is not supported: a microVM runs in the background and is reached with exec")
	}
	if args.stopSignal != nil {
		signal := *args.stopSignal
		if !strings.EqualFold(signal, "KILL") && signal != "9" && !strings.EqualFold(signal, "SIGKILL") {
			return 0, fmt.Errorf("unsupported --stop-signal %s: stopping a VM always kills it; use KILL", microvmDebugString(signal))
		}
	}
	if err := MicrovmValidateName("container", args.name); err != nil {
		return 0, err
	}
	if args.memory == nil {
		return 0, errors.New("--memory is required: a microVM's memory is reserved from the Nitro pool")
	}
	memory, err := microvmCLIParseMemory(*args.memory)
	if err != nil {
		return 0, err
	}
	pull := MicrovmPullPolicyMissing
	if args.pull != nil {
		if pull, err = MicrovmParsePullPolicy(*args.pull); err != nil {
			return 0, err
		}
	}
	labels, err := microvmCLIParseLabels(args.labels)
	if err != nil {
		return 0, err
	}
	env := microvmCLIResolveEnv(args.env)
	volumes := []MicrovmVolumeArg{}
	for _, text := range args.volumes {
		volume, err := microvmCLIParseVolume(text)
		if err != nil {
			return 0, err
		}
		volumes = append(volumes, volume)
	}
	id, err := MicrovmVMRun(layout, MicrovmRunSpec{
		Name:        args.name,
		Image:       args.image,
		Platform:    args.platform,
		Pull:        pull,
		Labels:      labels,
		CPUs:        args.cpus,
		MemoryBytes: memory,
		Env:         env,
		User:        args.user,
		Workdir:     args.workdir,
		Volumes:     volumes,
		Command:     args.command,
	})
	if err != nil {
		return 0, err
	}
	fmt.Println(id)
	return 0, nil
}

func microvmCLIExec(layout MicrovmLayout, args microvmExecArgs) (int, error) {
	_, client, err := MicrovmAgentFor(layout, args.container)
	if err != nil {
		return 0, err
	}
	request := MicrovmExecRequest{
		Argv:   args.command,
		Env:    microvmCLIResolveEnv(args.env),
		Cwd:    args.workdir,
		User:   args.user,
		Stdin:  args.interactive,
		Detach: args.detach,
	}
	if args.detach {
		if args.interactive {
			_ = client.transport.Close()
			return 0, errors.New("--detach and --interactive cannot be combined")
		}
		if _, err := client.ExecDetached(request); err != nil {
			return 0, err
		}
		return 0, nil
	}
	var stdin io.Reader
	if args.interactive {
		stdin = os.Stdin
	}
	exit, err := client.Exec(request, stdin, os.Stdout, os.Stderr)
	if err != nil {
		return 0, err
	}
	return int(exit.ShellStatus()), nil
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
	sourceName, sourcePath, sourceInside := microvmCLISplitContainerOperand(args.source)
	destinationName, destinationPath, destinationInside := microvmCLISplitContainerOperand(args.destination)
	switch {
	case !sourceInside && destinationInside:
		// `SRC/.` copies the directory's contents, as in Podman.
		source, mode := MicrovmSplitContentsOperand(args.source)
		_, client, err := MicrovmAgentFor(layout, destinationName)
		if err != nil {
			return 0, err
		}
		if err := client.CopyIn(source, destinationPath, mode); err != nil {
			return 0, err
		}
	case sourceInside && !destinationInside:
		source, mode := MicrovmSplitContentsOperand(sourcePath)
		_, client, err := MicrovmAgentFor(layout, sourceName)
		if err != nil {
			return 0, err
		}
		if err := client.CopyOut(source, args.destination, MicrovmSudoOwner(), mode); err != nil {
			return 0, err
		}
	case sourceInside && destinationInside:
		return 0, errors.New("copying between two containers is not supported")
	default:
		return 0, errors.New("one of the cp operands must be name:path inside a container")
	}
	return 0, nil
}

func microvmCLIVolumeCommand(layout MicrovmLayout, command microvmVolumeCommand) (int, error) {
	switch command := command.(type) {
	case microvmVolumeCommandCreate:
		labels, err := microvmCLIParseLabels(command.labels)
		if err != nil {
			return 0, err
		}
		if _, err := MicrovmVolumeCreate(layout, command.name, labels); err != nil {
			return 0, err
		}
		fmt.Println(command.name)
		return 0, nil
	case microvmVolumeCommandRm:
		for _, name := range command.names {
			if err := MicrovmVolumeRemove(layout, name, command.force); err != nil {
				return 0, err
			}
			fmt.Println(name)
		}
		return 0, nil
	case microvmVolumeCommandLs:
		if err := layout.Ensure(); err != nil {
			return 0, err
		}
		metadata, err := MicrovmVolumeMetadataList(layout)
		if err != nil {
			return 0, err
		}
		volumes := make([]jsonvalue.Object, len(metadata))
		for i, volume := range metadata {
			volumes[i] = microvmCLIDescribeVolume(layout, volume)
		}
		switch {
		case command.format == nil:
			fmt.Println("DRIVER    VOLUME NAME")
			for _, volume := range volumes {
				name, _ := microvmObjectGet(volume, "Name").(string)
				fmt.Printf("local     %s\n", name)
			}
		case *command.format == "json":
			list := make([]any, len(volumes))
			for i, volume := range volumes {
				list[i] = volume
			}
			text, err := microvmPrettyJSON(list)
			if err != nil {
				return 0, err
			}
			fmt.Println(text)
		default:
			for _, volume := range volumes {
				rendered, err := MicrovmTemplateRender(*command.format, volume)
				if err != nil {
					return 0, err
				}
				fmt.Print(rendered)
			}
		}
		return 0, nil
	case microvmVolumeCommandInspect:
		if err := MicrovmValidateName("volume", command.name); err != nil {
			return 0, err
		}
		metadata, err := MicrovmVolumeMetadataRead(layout, command.name)
		if err != nil {
			return 0, err
		}
		if metadata == nil {
			return 0, fmt.Errorf("no volume with name %s found: no such volume", microvmDebugString(command.name))
		}
		if err := microvmCLIPrintDescribed(microvmCLIDescribeVolume(layout, *metadata), command.format); err != nil {
			return 0, err
		}
		return 0, nil
	case microvmVolumeCommandExists:
		if err := MicrovmValidateName("volume", command.name); err != nil {
			return 0, err
		}
		metadata, err := MicrovmVolumeMetadataRead(layout, command.name)
		if err != nil {
			return 0, err
		}
		if metadata != nil {
			return 0, nil
		}
		return 1, nil
	}
	return 0, fmt.Errorf("unknown volume command %T", command)
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
