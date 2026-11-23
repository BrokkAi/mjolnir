// Port of mj-core/src/worker_launch.rs.

// Launch descriptions the controller writes and the target-side worker reads.
//
// These are plain data types and the constants they name: the file layout of
// a worker root, the launch configuration for a primary session and for a
// reviewer child, and how a harness learns about its MCP servers. They carry
// no process, network, or worker-runtime behaviour, so both sides of the
// relay can depend on them without depending on each other.

package core

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"strings"
)

// InstalledHarnessHome is the harness home an installed launch configuration
// names; ok is false when there is no installed configuration.
//
// Read loosely, as JSON, because it was written by an earlier release: only
// the fields that locate the home matter here. A configuration that states no
// home was started with the harness's home variable.
func InstalledHarnessHome(launchPath string) (home string, ok bool, err error) {
	body, err := os.ReadFile(launchPath)
	if errors.Is(err, fs.ErrNotExist) {
		return "", false, nil
	}
	if err != nil {
		return "", false, err
	}
	var launch any
	if err := json.Unmarshal(body, &launch); err != nil {
		return "", false, err
	}
	fields, _ := launch.(map[string]any)
	if stated, ok := fields["harness_home"].(string); ok && stated != "" {
		return stated, true, nil
	}
	rawHarness, present := fields["harness"]
	if !present {
		return "", false, nil
	}
	encoded, err := json.Marshal(rawHarness)
	if err != nil {
		return "", false, nil
	}
	var harness HarnessKind
	if err := json.Unmarshal(encoded, &harness); err != nil {
		return "", false, nil
	}
	environment, _ := fields["environment"].(map[string]any)
	value, ok := environment[harness.HomeEnv()].(string)
	if !ok {
		return "", false, nil
	}
	return harness.HomeFromEnvironment(value), true, nil
}

// HarnessRuntimePolicy is where a worker's ACP adapter and harness executable
// come from. harnessinstall.Locate turns a policy into a launch.
type HarnessRuntimePolicy int

const (
	// HarnessRuntimePolicyAmbient is the target's own installation, for
	// example a container target's image, with Mjolnir's managed installation
	// when it is missing or incompatible. The default.
	HarnessRuntimePolicyAmbient HarnessRuntimePolicy = iota
	// HarnessRuntimePolicyManaged is Mjolnir's managed installation of the
	// compiled pin. Serialized as "managed_remote", also read as "managed".
	HarnessRuntimePolicyManaged
	// HarnessRuntimePolicyImage is only the pinned harnesses of the image
	// this process runs in: the daemon container's agent-dev image. A missing
	// or incompatible harness is an error; nothing is installed. Only a
	// container daemon writes it, for workers in its own container, which run
	// the image's own build.
	HarnessRuntimePolicyImage
)

func (p HarnessRuntimePolicy) MarshalJSON() ([]byte, error) {
	switch p {
	case HarnessRuntimePolicyAmbient:
		return []byte(`"ambient"`), nil
	case HarnessRuntimePolicyManaged:
		return []byte(`"managed_remote"`), nil
	case HarnessRuntimePolicyImage:
		return []byte(`"image"`), nil
	}
	return nil, fmt.Errorf("invalid harness runtime policy %d", int(p))
}

func (p *HarnessRuntimePolicy) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch text {
	case "ambient":
		*p = HarnessRuntimePolicyAmbient
	case "managed_remote", "managed":
		*p = HarnessRuntimePolicyManaged
	case "image":
		*p = HarnessRuntimePolicyImage
	default:
		return fmt.Errorf("unknown variant `%s`, expected one of `ambient`, `managed_remote`, `image`", text)
	}
	return nil
}

func (p HarnessRuntimePolicy) IsAmbient() bool { return p == HarnessRuntimePolicyAmbient }

// MayInstall reports whether this source may download Mjolnir's managed
// harness or Node.js.
func (p HarnessRuntimePolicy) MayInstall() bool { return p != HarnessRuntimePolicyImage }

// WorkerOwnership is serialized with deny_unknown_fields.
type WorkerOwnership struct {
	Version uint32 `json:"version"`
	// NoGithubCredentials retains the accepted credential policy if an
	// orphan is adopted.
	NoGithubCredentials bool   `json:"no_github_credentials,omitempty"`
	WorkspaceID         string `json:"workspace_id"`
	SessionID           string `json:"session_id"`
	ProfileID           string `json:"profile_id"`
	BundleID            string `json:"bundle_id"`
	TargetTemplateID    string `json:"target_template_id"`
	// InstanceID is the instance that created the worker
	// (InstanceIdentity). Markers written before version 3 omit it.
	InstanceID *string `json:"instance_id,omitempty"`
}

func (o *WorkerOwnership) UnmarshalJSON(data []byte) error {
	type plain WorkerOwnership
	value := plain{WorkspaceID: defaultWorkerWorkspaceID()}
	if err := workerLaunchDecodeStrict(data, &value, "version", "session_id", "profile_id", "bundle_id", "target_template_id"); err != nil {
		return err
	}
	*o = WorkerOwnership(value)
	return nil
}

const WorkerOwnershipVersion uint32 = 3

func (o *WorkerOwnership) Write(path string) error {
	body, err := json.Marshal(o)
	if err != nil {
		return err
	}
	return AtomicWrite(path, body)
}

func defaultWorkerWorkspaceID() string { return DefaultWorkspaceID }

func defaultAgentMailboxesEnabled() bool { return true }

func defaultSubtleSubagents() bool { return true }

func isDefaultSubtleSubagents(value bool) bool { return value }

// SessionGitConfigIncludePath is an internal launch directive: include this
// absolute Git config file in the worker-owned session global config, then
// remove the key before harness use.
const SessionGitConfigIncludePath = "MJ_SESSION_GIT_CONFIG_INCLUDE_PATH"

// SessionManagedSubagentEnv is a controller-authored environment marker so
// old launch.json readers remain compatible while a new worker can
// distinguish a managed child whose historical handback tool is disabled.
const SessionManagedSubagentEnv = "MJ_SESSION_MANAGED_SUBAGENT"

// SessionRawLaunchRecordEnv is a controller-authored environment marker:
// record a raw Git launch baseline (`mj/sessions/<session-id>/launch`)
// before starting the harness if absent.
const SessionRawLaunchRecordEnv = "MJ_SESSION_RAW_LAUNCH_RECORD"

// SessionMessageMCPEnv is a controller-authored environment marker: the
// controller staged the message-only `mj-agents` registration for this
// top-level session. A worker grants that role only with this marker, so a
// newer worker resuming a launch written by an older controller does not
// demand a registration that was never staged.
const SessionMessageMCPEnv = "MJ_SESSION_MESSAGE_MCP"

// SessionProjectToolsRoots is controller-authored JSON checkout roots for
// container-only project tool installation. The worker consumes this
// directive before starting a harness.
const SessionProjectToolsRoots = "MJ_SESSION_PROJECT_TOOLS_ROOTS"

// WorkerRunMode is whether this worker executes the harness or only
// preserves recovered state.
type WorkerRunMode int

const (
	WorkerRunModeHarness WorkerRunMode = iota
	WorkerRunModeCheckpointOnly
)

func (m WorkerRunMode) MarshalJSON() ([]byte, error) {
	switch m {
	case WorkerRunModeHarness:
		return []byte(`"harness"`), nil
	case WorkerRunModeCheckpointOnly:
		return []byte(`"checkpoint_only"`), nil
	}
	return nil, fmt.Errorf("invalid worker run mode %d", int(m))
}

func (m *WorkerRunMode) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch text {
	case "harness":
		*m = WorkerRunModeHarness
	case "checkpoint_only":
		*m = WorkerRunModeCheckpointOnly
	default:
		return fmt.Errorf("unknown variant `%s`, expected `harness` or `checkpoint_only`", text)
	}
	return nil
}

// WorkerLaunchConfig is serialized with deny_unknown_fields; see its
// MarshalJSON and UnmarshalJSON for the defaults, aliases and omissions.
type WorkerLaunchConfig struct {
	// CPUAccounting is chosen from the target locator, never from ambient
	// container detection.
	CPUAccounting CpuAccountingMode
	// NoGithubCredentials is the accepted session policy; do not seed or
	// accept GitHub authentication.
	NoGithubCredentials bool
	// InitialCommands is ordered input journaled before this worker accepts
	// external commands. Kept in the launch description so a crash during
	// initialization can finish the same prefix under its original command
	// IDs.
	InitialCommands []InitialWorkerCommand
	ReviewerLaunch  *ReviewerLaunchConfig
	// GoalResumeRequest is the unique intent for a user-requested
	// Resume/Restart; recovery preserves it in the relay.
	GoalResumeRequest *string
	RunMode           WorkerRunMode
	SessionID         string
	// Subagents is the resolved delegation policy; independently controls
	// MCP exposure and native suppression. Also read as "subagent_tools".
	Subagents SubagentPolicy
	// HandbackTool is whether this is a sub-agent child that hands its report
	// back through the `mj-agents` MCP server's `handback` tool. Children
	// suppress native delegation.
	HandbackTool bool
	// SubagentKind is the explicit child identity; older launch files deliver
	// handbacks unchecked.
	SubagentKind *SubagentKind
	// AgentMailboxesEnabled is a legacy wire flag retained for older workers.
	// New workers ignore it. Mailbox delivery and hooks are always present in
	// protocol-41 workers. Defaults to true.
	AgentMailboxesEnabled bool
	// SubtleSubagents is whether a delegating parent may choose the subtle
	// task type. False when only the straightforward type is configured: the
	// spawn tool then has no `task_type` and every child uses the
	// straightforward choice. Launch files written before this field offered
	// both types. Defaults to true and is written only when false.
	SubtleSubagents bool
	// Oracle is whether the worker's `mj-agents` server lists `ask_oracle`.
	// Written only when true.
	Oracle bool
	// InitialModel is the model this session was created for, when its
	// creator chose one: a sub-agent's spawn model. The harness opens a new
	// session on it rather than on the profile default; once the session has
	// accepted a model, that one wins.
	InitialModel *string
	// ReviewCapture is accepted from older daemon launch files; no longer
	// used or written.
	ReviewCapture bool
	ReviewTrigger ReviewTrigger
	// TargetEnvironment is explicit target settings shared by primary and
	// reviewer processes.
	TargetEnvironment map[string]string
	// SeedImageEnvironment restores the container image's declared `ENV` onto
	// the discovered login environment before applying TargetEnvironment.
	// True only for container targets, where the worker's own environment is
	// the image's; on a bare or localhost target the ambient environment is
	// the user's shell and must not be carried.
	SeedImageEnvironment bool
	Harness              HarnessKind
	// HarnessHome is the harness home this session reads and writes inside
	// the target.
	//
	// The worker locates credentials and skills through it, so it is stated
	// rather than read back out of Environment: a harness whose home cannot
	// be scoped by environment sets no home variable at all. Configs persisted
	// by older releases omit the field; the worker then falls back to the
	// harness's home variable, which those releases always set.
	HarnessHome string
	// AuthenticationMarker is the path, relative to the staged harness home,
	// of the file that proves authentication, such as `auth.json` or
	// `credentials/kimi-code.json`. An API-key profile is proven by its
	// harness configuration file rather than a credential file, so the
	// controller decides the name and the worker does not re-derive it.
	// Configs persisted by older releases omit the field; the worker then
	// falls back to the harness kind's own marker.
	AuthenticationMarker *string
	BridgeCommand        string
	BridgeArgs           []string
	HarnessRuntime       HarnessRuntimePolicy
	Environment          map[string]string
	// ExcludedEnvironment is the variables the worker removes from the
	// harness environment after it adds the target's own login environment,
	// so that a value set on the target (a shell profile, the image, the
	// target's settings) cannot reach the harness either. The controller has
	// already left them out of Environment. See
	// HarnessProfile.ExcludeHarnessEnvironment.
	ExcludedEnvironment   []string
	Cwd                   string
	AdditionalDirectories []string
	NativeSessionID       *string
	ProjectMemory         *ProjectMemoryLaunchConfig
	// ExecutionPolicy is target-level policy translated into
	// harness-specific controls by the worker. Localhost and guardian SSH
	// targets preserve configured approvals for harnesses that support them.
	// Other targets run unconstrained. Also read as the legacy boolean
	// "force_unrestricted_mode".
	ExecutionPolicy ExecutionPolicy
	// ClaudePermissionMode is the Claude profile's `claude_permission_mode`,
	// which picks Claude's mode under configured approvals. Written only when
	// it is not Auto, so launches that keep the default stay readable by
	// older workers.
	ClaudePermissionMode ClaudePermissionMode
	// MjolnirGuardian is the profile's `mjolnir_guardian`, which decides
	// whether the worker reviews a Kimi Code, OpenCode, or DSH session's
	// approvals under configured approvals. Written only when off, so
	// launches that keep the default stay readable by older workers.
	// Defaults to true.
	MjolnirGuardian bool
	// ObservePolicy is non-nil when an older daemon launch file carried Brokk
	// Observe guardrail settings here; no longer used or written. This struct
	// rejects unknown fields, so dropping the name would make a new worker
	// refuse those launches. The contents (including a bearer token) are
	// discarded while parsing.
	ObservePolicy *struct{}
}

type workerLaunchConfigWireOut struct {
	CPUAccounting         *CpuAccountingMode         `json:"cpu_accounting,omitempty"`
	NoGithubCredentials   bool                       `json:"no_github_credentials,omitempty"`
	InitialCommands       []InitialWorkerCommand     `json:"initial_commands,omitempty"`
	ReviewerLaunch        *ReviewerLaunchConfig      `json:"reviewer_launch,omitempty"`
	GoalResumeRequest     *string                    `json:"goal_resume_request,omitempty"`
	RunMode               WorkerRunMode              `json:"run_mode"`
	SessionID             string                     `json:"session_id"`
	Subagents             SubagentPolicy             `json:"subagents"`
	HandbackTool          bool                       `json:"handback_tool"`
	SubagentKind          *SubagentKind              `json:"subagent_kind,omitempty"`
	AgentMailboxesEnabled bool                       `json:"agent_mailboxes_enabled"`
	SubtleSubagents       *bool                      `json:"subtle_subagents,omitempty"`
	Oracle                bool                       `json:"oracle,omitempty"`
	InitialModel          *string                    `json:"initial_model,omitempty"`
	ReviewTrigger         ReviewTrigger              `json:"review_trigger"`
	TargetEnvironment     map[string]string          `json:"target_environment,omitempty"`
	SeedImageEnvironment  bool                       `json:"seed_image_environment"`
	Harness               HarnessKind                `json:"harness"`
	HarnessHome           string                     `json:"harness_home"`
	AuthenticationMarker  *string                    `json:"authentication_marker,omitempty"`
	BridgeCommand         string                     `json:"bridge_command"`
	BridgeArgs            []string                   `json:"bridge_args"`
	HarnessRuntime        HarnessRuntimePolicy       `json:"harness_runtime,omitempty"`
	Environment           map[string]string          `json:"environment"`
	ExcludedEnvironment   []string                   `json:"excluded_environment,omitempty"`
	Cwd                   string                     `json:"cwd"`
	AdditionalDirectories []string                   `json:"additional_directories"`
	NativeSessionID       *string                    `json:"native_session_id"`
	ProjectMemory         *ProjectMemoryLaunchConfig `json:"project_memory,omitempty"`
	ExecutionPolicy       ExecutionPolicy            `json:"execution_policy"`
	ClaudePermissionMode  *ClaudePermissionMode      `json:"claude_permission_mode,omitempty"`
	MjolnirGuardian       *bool                      `json:"mjolnir_guardian,omitempty"`
}

func (c WorkerLaunchConfig) MarshalJSON() ([]byte, error) {
	out := workerLaunchConfigWireOut{
		NoGithubCredentials:   c.NoGithubCredentials,
		InitialCommands:       c.InitialCommands,
		ReviewerLaunch:        c.ReviewerLaunch,
		GoalResumeRequest:     c.GoalResumeRequest,
		RunMode:               c.RunMode,
		SessionID:             c.SessionID,
		Subagents:             c.Subagents,
		HandbackTool:          c.HandbackTool,
		SubagentKind:          c.SubagentKind,
		AgentMailboxesEnabled: c.AgentMailboxesEnabled,
		Oracle:                c.Oracle,
		InitialModel:          c.InitialModel,
		ReviewTrigger:         c.ReviewTrigger,
		TargetEnvironment:     c.TargetEnvironment,
		SeedImageEnvironment:  c.SeedImageEnvironment,
		Harness:               c.Harness,
		HarnessHome:           c.HarnessHome,
		AuthenticationMarker:  c.AuthenticationMarker,
		BridgeCommand:         c.BridgeCommand,
		BridgeArgs:            workerLaunchNonNilSlice(c.BridgeArgs),
		HarnessRuntime:        c.HarnessRuntime,
		Environment:           workerLaunchNonNilMap(c.Environment),
		ExcludedEnvironment:   c.ExcludedEnvironment,
		Cwd:                   c.Cwd,
		AdditionalDirectories: workerLaunchNonNilSlice(c.AdditionalDirectories),
		NativeSessionID:       c.NativeSessionID,
		ProjectMemory:         c.ProjectMemory,
		ExecutionPolicy:       c.ExecutionPolicy,
	}
	if !c.CPUAccounting.IsProcessTree() {
		mode := c.CPUAccounting
		out.CPUAccounting = &mode
	}
	if !isDefaultSubtleSubagents(c.SubtleSubagents) {
		subtle := c.SubtleSubagents
		out.SubtleSubagents = &subtle
	}
	if !c.ClaudePermissionMode.IsAuto() {
		mode := c.ClaudePermissionMode
		out.ClaudePermissionMode = &mode
	}
	if !c.MjolnirGuardian {
		guardian := c.MjolnirGuardian
		out.MjolnirGuardian = &guardian
	}
	return json.Marshal(out)
}

type workerLaunchConfigWireIn struct {
	CPUAccounting         *CpuAccountingMode         `json:"cpu_accounting"`
	NoGithubCredentials   bool                       `json:"no_github_credentials"`
	InitialCommands       []InitialWorkerCommand     `json:"initial_commands"`
	ReviewerLaunch        *ReviewerLaunchConfig      `json:"reviewer_launch"`
	GoalResumeRequest     *string                    `json:"goal_resume_request"`
	RunMode               WorkerRunMode              `json:"run_mode"`
	SessionID             *string                    `json:"session_id"`
	Subagents             json.RawMessage            `json:"subagents"`
	SubagentTools         json.RawMessage            `json:"subagent_tools"`
	HandbackTool          bool                       `json:"handback_tool"`
	SubagentKind          *SubagentKind              `json:"subagent_kind"`
	AgentMailboxesEnabled *bool                      `json:"agent_mailboxes_enabled"`
	SubtleSubagents       *bool                      `json:"subtle_subagents"`
	Oracle                bool                       `json:"oracle"`
	InitialModel          *string                    `json:"initial_model"`
	ReviewCapture         bool                       `json:"review_capture"`
	ReviewTrigger         *ReviewTrigger             `json:"review_trigger"`
	TargetEnvironment     map[string]string          `json:"target_environment"`
	SeedImageEnvironment  bool                       `json:"seed_image_environment"`
	Harness               *HarnessKind               `json:"harness"`
	HarnessHome           string                     `json:"harness_home"`
	AuthenticationMarker  *string                    `json:"authentication_marker"`
	BridgeCommand         *string                    `json:"bridge_command"`
	BridgeArgs            *[]string                  `json:"bridge_args"`
	HarnessRuntime        HarnessRuntimePolicy       `json:"harness_runtime"`
	Environment           *map[string]string         `json:"environment"`
	ExcludedEnvironment   []string                   `json:"excluded_environment"`
	Cwd                   *string                    `json:"cwd"`
	AdditionalDirectories []string                   `json:"additional_directories"`
	NativeSessionID       *string                    `json:"native_session_id"`
	ProjectMemory         *ProjectMemoryLaunchConfig `json:"project_memory"`
	ExecutionPolicy       json.RawMessage            `json:"execution_policy"`
	ForceUnrestrictedMode json.RawMessage            `json:"force_unrestricted_mode"`
	ClaudePermissionMode  *ClaudePermissionMode      `json:"claude_permission_mode"`
	MjolnirGuardian       *bool                      `json:"mjolnir_guardian"`
	ObservePolicy         json.RawMessage            `json:"observe_policy"`
}

func (c *WorkerLaunchConfig) UnmarshalJSON(data []byte) error {
	var in workerLaunchConfigWireIn
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&in); err != nil {
		return err
	}
	switch {
	case in.SessionID == nil:
		return fmt.Errorf("missing field `session_id`")
	case in.Harness == nil:
		return fmt.Errorf("missing field `harness`")
	case in.BridgeCommand == nil:
		return fmt.Errorf("missing field `bridge_command`")
	case in.BridgeArgs == nil:
		return fmt.Errorf("missing field `bridge_args`")
	case in.Environment == nil:
		return fmt.Errorf("missing field `environment`")
	case in.Cwd == nil:
		return fmt.Errorf("missing field `cwd`")
	}
	subagentsRaw := in.Subagents
	if in.SubagentTools != nil {
		if subagentsRaw != nil {
			return fmt.Errorf("duplicate field `subagents`")
		}
		subagentsRaw = in.SubagentTools
	}
	if subagentsRaw == nil {
		subagentsRaw = json.RawMessage("null")
	}
	subagents, err := DeserializeLaunchPolicy(subagentsRaw)
	if err != nil {
		return err
	}
	policyRaw := in.ExecutionPolicy
	if in.ForceUnrestrictedMode != nil {
		if policyRaw != nil {
			return fmt.Errorf("duplicate field `execution_policy`")
		}
		policyRaw = in.ForceUnrestrictedMode
	}
	if policyRaw == nil {
		return fmt.Errorf("missing field `execution_policy`")
	}
	policy, err := deserializeExecutionPolicy(policyRaw)
	if err != nil {
		return err
	}
	out := WorkerLaunchConfig{
		CPUAccounting:         CpuAccountingModeProcessTree,
		NoGithubCredentials:   in.NoGithubCredentials,
		InitialCommands:       in.InitialCommands,
		ReviewerLaunch:        in.ReviewerLaunch,
		GoalResumeRequest:     in.GoalResumeRequest,
		RunMode:               in.RunMode,
		SessionID:             *in.SessionID,
		Subagents:             subagents,
		HandbackTool:          in.HandbackTool,
		SubagentKind:          in.SubagentKind,
		AgentMailboxesEnabled: defaultAgentMailboxesEnabled(),
		SubtleSubagents:       defaultSubtleSubagents(),
		Oracle:                in.Oracle,
		InitialModel:          in.InitialModel,
		ReviewCapture:         in.ReviewCapture,
		ReviewTrigger:         ReviewTriggerOff,
		TargetEnvironment:     in.TargetEnvironment,
		SeedImageEnvironment:  in.SeedImageEnvironment,
		Harness:               *in.Harness,
		HarnessHome:           in.HarnessHome,
		AuthenticationMarker:  in.AuthenticationMarker,
		BridgeCommand:         *in.BridgeCommand,
		BridgeArgs:            *in.BridgeArgs,
		HarnessRuntime:        in.HarnessRuntime,
		Environment:           *in.Environment,
		ExcludedEnvironment:   in.ExcludedEnvironment,
		Cwd:                   *in.Cwd,
		AdditionalDirectories: in.AdditionalDirectories,
		NativeSessionID:       in.NativeSessionID,
		ProjectMemory:         in.ProjectMemory,
		ExecutionPolicy:       policy,
		ClaudePermissionMode:  ClaudePermissionModeAuto,
		MjolnirGuardian:       true,
	}
	if in.CPUAccounting != nil {
		out.CPUAccounting = *in.CPUAccounting
	}
	if in.AgentMailboxesEnabled != nil {
		out.AgentMailboxesEnabled = *in.AgentMailboxesEnabled
	}
	if in.SubtleSubagents != nil {
		out.SubtleSubagents = *in.SubtleSubagents
	}
	if in.ReviewTrigger != nil {
		out.ReviewTrigger = *in.ReviewTrigger
	}
	if in.ClaudePermissionMode != nil {
		out.ClaudePermissionMode = *in.ClaudePermissionMode
	}
	if in.MjolnirGuardian != nil {
		out.MjolnirGuardian = *in.MjolnirGuardian
	}
	if in.ObservePolicy != nil && string(bytes.TrimSpace(in.ObservePolicy)) != "null" {
		out.ObservePolicy = &struct{}{}
	}
	*c = out
	return nil
}

// InitialWorkerCommand is serialized with deny_unknown_fields.
type InitialWorkerCommand struct {
	CommandID string       `json:"command_id"`
	Command   RelayCommand `json:"command"`
	// Optional configuration may be refused without abandoning the brief.
	Optional bool `json:"optional,omitempty"`
}

func (c *InitialWorkerCommand) UnmarshalJSON(data []byte) error {
	var wire struct {
		CommandID string          `json:"command_id"`
		Command   json.RawMessage `json:"command"`
		Optional  bool            `json:"optional"`
	}
	if err := workerLaunchDecodeStrict(data, &wire, "command_id", "command"); err != nil {
		return err
	}
	command, err := UnmarshalRelayCommandJSON(wire.Command)
	if err != nil {
		return err
	}
	*c = InitialWorkerCommand{CommandID: wire.CommandID, Command: command, Optional: wire.Optional}
	return nil
}

// deserializeExecutionPolicy reads the current policy name or the legacy
// boolean (true is unconstrained).
func deserializeExecutionPolicy(data []byte) (ExecutionPolicy, error) {
	var legacy bool
	if err := json.Unmarshal(data, &legacy); err == nil {
		if legacy {
			return ExecutionPolicyUnconstrained, nil
		}
		return ExecutionPolicyConfiguredApprovals, nil
	}
	var policy ExecutionPolicy
	if err := json.Unmarshal(data, &policy); err != nil {
		return policy, fmt.Errorf("data did not match any variant of untagged enum WirePolicy")
	}
	return policy, nil
}

// ProjectMemoryLaunchConfig is serialized with deny_unknown_fields.
type ProjectMemoryLaunchConfig struct {
	// HistorySocket is the private worker control socket for
	// controller-backed history tools.
	HistorySocket *string `json:"history_socket,omitempty"`
	// ProjectKey is the stable controller-derived identity for this
	// repository or bundle.
	ProjectKey string `json:"project_key"`
	// Root is the target-side replica for file-based project notes and
	// controller sync.
	Root string `json:"root"`
	// BaselineRoot is the session-private copy of the canonical tree from the
	// last successful synchronization, used as the three-way merge base.
	BaselineRoot string `json:"baseline_root"`
	// RepositoryRoots maps bundle repository IDs to the roots presented over
	// ACP.
	RepositoryRoots map[string]string `json:"repository_roots,omitempty"`
	// MCPDelivery is how the harness learns about the project-history MCP
	// server. Most ACP adapters accept a stdio server in `session/new`;
	// adapters that need harness-specific runtime metadata receive it through
	// their staged profile instead. Omitted when Acp.
	MCPDelivery ProjectMemoryMcpDelivery `json:"mcp_delivery,omitempty"`
}

func (c *ProjectMemoryLaunchConfig) UnmarshalJSON(data []byte) error {
	type plain ProjectMemoryLaunchConfig
	var value plain
	if err := workerLaunchDecodeStrict(data, &value, "project_key", "root"); err != nil {
		return err
	}
	*c = ProjectMemoryLaunchConfig(value)
	return nil
}

type ProjectMemoryMcpDelivery int

const (
	ProjectMemoryMcpDeliveryAcp ProjectMemoryMcpDelivery = iota
	ProjectMemoryMcpDeliveryHarnessProfile
)

func (d ProjectMemoryMcpDelivery) MarshalJSON() ([]byte, error) {
	switch d {
	case ProjectMemoryMcpDeliveryAcp:
		return []byte(`"acp"`), nil
	case ProjectMemoryMcpDeliveryHarnessProfile:
		return []byte(`"harness_profile"`), nil
	}
	return nil, fmt.Errorf("invalid project memory MCP delivery %d", int(d))
}

func (d *ProjectMemoryMcpDelivery) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch text {
	case "acp":
		*d = ProjectMemoryMcpDeliveryAcp
	case "harness_profile":
		*d = ProjectMemoryMcpDeliveryHarnessProfile
	default:
		return fmt.Errorf("unknown variant `%s`, expected `acp` or `harness_profile`", text)
	}
	return nil
}

func (d ProjectMemoryMcpDelivery) isAcp() bool { return d == ProjectMemoryMcpDeliveryAcp }

// ReviewChildRole is the output contract a review child receives from its
// MCP server.
type ReviewChildRole int

const (
	ReviewChildRoleReviewer ReviewChildRole = iota
	ReviewChildRoleBriefWriter
)

func (r ReviewChildRole) MarshalJSON() ([]byte, error) {
	switch r {
	case ReviewChildRoleReviewer:
		return []byte(`"reviewer"`), nil
	case ReviewChildRoleBriefWriter:
		return []byte(`"brief_writer"`), nil
	}
	return nil, fmt.Errorf("invalid review child role %d", int(r))
}

func (r *ReviewChildRole) UnmarshalJSON(data []byte) error {
	var text string
	if err := json.Unmarshal(data, &text); err != nil {
		return err
	}
	switch text {
	case "reviewer":
		*r = ReviewChildRoleReviewer
	case "brief_writer":
		*r = ReviewChildRoleBriefWriter
	default:
		return fmt.Errorf("unknown variant `%s`, expected `reviewer` or `brief_writer`", text)
	}
	return nil
}

func (r ReviewChildRole) isReviewer() bool { return r == ReviewChildRoleReviewer }

// ReviewerLaunchConfig is harness launch settings for a reviewer child.
//
// The reviewer shares the primary's target and working directory and nothing
// else: its harness home is a fresh copy of the chosen profile, staged under
// its own worker root, and the worker sets that home itself so a controller
// can never point a reviewer at the primary's credentials. Serialized with
// deny_unknown_fields.
type ReviewerLaunchConfig struct {
	// Role is omitted for reviewers so older workers keep reading their
	// launches. Older workers reject a writer's unknown field before starting
	// a harness.
	Role ReviewChildRole `json:"role,omitempty"`
	// ProfileID is the configured profile this reviewer was staged from, for
	// display and for deciding whether a saved reviewer still matches the
	// user's choice.
	ProfileID     string      `json:"profile_id"`
	Harness       HarnessKind `json:"harness"`
	BridgeCommand string      `json:"bridge_command"`
	BridgeArgs    []string    `json:"bridge_args"`
	// Environment is the harness environment without its home variable: the
	// worker fills that in from the staged reviewer directory it owns.
	Environment map[string]string `json:"environment"`
	// ExcludedEnvironment is the variables the worker removes from the
	// reviewer's environment after it adds the target's login environment;
	// see WorkerLaunchConfig.ExcludedEnvironment.
	ExcludedEnvironment []string        `json:"excluded_environment,omitempty"`
	ExecutionPolicy     ExecutionPolicy `json:"execution_policy"`
	// Model to apply once the session opens, or nil to keep the profile's
	// default. Explicit selections must be supported by the target adapter.
	Model  *string `json:"model,omitempty"`
	Effort *string `json:"effort,omitempty"`
	// FastMode is best-effort fast mode, applied only when the harness
	// advertises it.
	FastMode *bool `json:"fast_mode,omitempty"`
	// Generation is a legacy launch field retained so saved worker
	// configurations remain readable.
	Generation uint64 `json:"generation"`
	// MCPServers is additional configured MCP servers this reviewer gets.
	MCPServers []ReviewMcpServer `json:"mcp_servers,omitempty"`
}

func (c ReviewerLaunchConfig) MarshalJSON() ([]byte, error) {
	type plain ReviewerLaunchConfig
	value := plain(c)
	value.BridgeArgs = workerLaunchNonNilSlice(value.BridgeArgs)
	value.Environment = workerLaunchNonNilMap(value.Environment)
	return json.Marshal(value)
}

func (c *ReviewerLaunchConfig) UnmarshalJSON(data []byte) error {
	type plain ReviewerLaunchConfig
	var value plain
	if err := workerLaunchDecodeStrict(data, &value, "profile_id", "harness", "bridge_command", "bridge_args", "execution_policy"); err != nil {
		return err
	}
	*c = ReviewerLaunchConfig(value)
	return nil
}

// ReviewMcpServer is one stdio MCP server a reviewing agent is given.
//
// How it reaches the harness depends on the harness: most accept a server in
// the ACP `session/new` request, while Claude and Kimi read their own
// configuration files, which the controller patches while staging the
// reviewer's profile. ReviewMcpDelivery is the single place that decides.
// Serialized with deny_unknown_fields.
type ReviewMcpServer struct {
	Name    string   `json:"name"`
	Command string   `json:"command"`
	Args    []string `json:"args"`
}

func (s ReviewMcpServer) MarshalJSON() ([]byte, error) {
	type plain ReviewMcpServer
	value := plain(s)
	value.Args = workerLaunchNonNilSlice(value.Args)
	return json.Marshal(value)
}

func (s *ReviewMcpServer) UnmarshalJSON(data []byte) error {
	type plain ReviewMcpServer
	var value plain
	if err := workerLaunchDecodeStrict(data, &value, "name", "command"); err != nil {
		return err
	}
	*s = ReviewMcpServer(value)
	return nil
}

// ReviewMcpDelivery is how a harness learns about a reviewing agent's MCP
// servers.
type ReviewMcpDelivery int

const (
	// ReviewMcpDeliveryAcp: attached to the ACP `session/new` request.
	ReviewMcpDeliveryAcp ReviewMcpDelivery = iota
	// ReviewMcpDeliveryHarnessProfile: written into the staged profile the
	// harness reads at startup.
	ReviewMcpDeliveryHarnessProfile
)

// ReviewMcpDeliveryForHarness: Claude and Kimi both ignore servers offered
// over ACP -- Claude is not given them at all (see `project_history_mcp` in
// `src/acp.rs`), and Kimi needs runtime metadata its own schema carries -- so
// both are configured through their staged profile instead.
func ReviewMcpDeliveryForHarness(harness HarnessKind) ReviewMcpDelivery {
	switch harness {
	case HarnessKindClaude, HarnessKindKimi:
		return ReviewMcpDeliveryHarnessProfile
	}
	return ReviewMcpDeliveryAcp
}

// ResolveSubagentMCPRole resolves the `mj-agents` server role from the
// launch values that control whether its durable request queue exists in the
// worker.
func ResolveSubagentMCPRole(subagents *SubagentPolicy, managedSubagent, handbackTool, legacyAgentMailboxesEnabled, messageMCPGranted bool) (SubagentMcpRole, bool) {
	if role, ok := subagents.ParentRole(); ok {
		return role, true
	}
	if managedSubagent {
		if handbackTool {
			return SubagentMcpRoleChild, true
		}
		return SubagentMcpRoleFetchOnly, true
	}
	if messageMCPGranted {
		return SubagentMcpRoleMessageOnly, true
	}
	var none SubagentMcpRole
	return none, false
}

// ReviewTools reports whether this session is offered the `mj-review` tools.
// The controller writes `review_trigger` as push only for sessions that may
// request review: top-level sessions and ordinary sub-agents, never reviewers
// or the oracle. This is the one owner of that fact.
func (c *WorkerLaunchConfig) ReviewTools() bool {
	return OffersReviewTools(c.ReviewTrigger, c.Harness)
}

// PreparesReview reports whether launch must prepare review state in the
// workspace: the session start pin for every session offered the review
// tools, and the push gate (or a notice that this harness cannot enforce one)
// for a primary.
func (c *WorkerLaunchConfig) PreparesReview() bool {
	return c.ReviewTrigger == ReviewTriggerPush &&
		c.ReviewerLaunch == nil &&
		(c.ReviewTools() || (!c.IsManagedSubagent() && !c.HandbackTool))
}

// ReviewGate reports whether pushes from this session are held to review:
// the pre-push hook, `MJ_REVIEW_GATE` and the session-start pin. Only
// top-level sessions are gated; a sub-agent gets the tools without the gate.
func (c *WorkerLaunchConfig) ReviewGate() bool {
	return c.ReviewTools() && !c.IsManagedSubagent() && !c.HandbackTool && c.ReviewerLaunch == nil
}

// IsManagedSubagent: managed ownership is independent of the MCP role and
// historical handback tools.
func (c *WorkerLaunchConfig) IsManagedSubagent() bool {
	return c.Environment[SessionManagedSubagentEnv] == "1"
}

// SubagentMCPRole: the `mj-agents` role is derived once from the launch's
// ownership and policy so Codex and Claude expose the same tools.
func (c *WorkerLaunchConfig) SubagentMCPRole() (SubagentMcpRole, bool) {
	return ResolveSubagentMCPRole(
		&c.Subagents,
		c.IsManagedSubagent(),
		c.HandbackTool,
		c.AgentMailboxesEnabled,
		c.Environment[SessionMessageMCPEnv] == "1" || c.ReviewGate(),
	)
}

// RequiresHarnessPreparation: downloads must finish before startup or an
// upgrade's idle reservation.
func (c *WorkerLaunchConfig) RequiresHarnessPreparation() bool {
	if c.HarnessRuntime == HarnessRuntimePolicyManaged {
		return true
	}
	if bridge, ok := NpmBridgeFor(c.Harness); ok && bridge.MatchesLauncher(c.BridgeCommand, c.BridgeArgs) {
		return true
	}
	return IsFallbackLaunch(c.Harness, c.ExecutionPolicy, c.BridgeCommand, c.BridgeArgs)
}

func WorkerLaunchConfigRead(path string) (WorkerLaunchConfig, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return WorkerLaunchConfig{}, fmt.Errorf("read worker launch config %s: %w", path, err)
	}
	parse := func() (WorkerLaunchConfig, error) {
		// Numbers keep their text so large integers survive the round trip.
		decoder := json.NewDecoder(bytes.NewReader(body))
		decoder.UseNumber()
		var value any
		if err := decoder.Decode(&value); err != nil {
			return WorkerLaunchConfig{}, err
		}
		if _, err := decoder.Token(); err != io.EOF {
			return WorkerLaunchConfig{}, errors.New("trailing characters after the launch config")
		}
		// Shipped workers keep launch.json across upgrades. The removed
		// runtime constraint and turn review's former Bifrost binary have no
		// effect, but must not block loading that file.
		if fields, ok := value.(map[string]any); ok {
			delete(fields, "expected_runtime_identity")
			delete(fields, "bifrost_binary")
		}
		cleaned, err := relayMarshalJSON(value)
		if err != nil {
			return WorkerLaunchConfig{}, err
		}
		var launch WorkerLaunchConfig
		err = json.Unmarshal(cleaned, &launch)
		return launch, err
	}
	launch, err := parse()
	if err != nil {
		wrapped := fmt.Errorf("parse worker launch config %s with worker build %s: %w", path, WorkerBuildID(), err)
		if strings.Contains(err.Error(), "unknown field") {
			return WorkerLaunchConfig{}, fmt.Errorf("the worker may be older than the daemon that wrote launch.json; install the worker built with the running mj: %w", wrapped)
		}
		return WorkerLaunchConfig{}, wrapped
	}
	// Daemons from 2.11 through 2.23 stamped their instance identity here.
	// For the default instance that is a data-directory fingerprint, which
	// every `mj` in the session then read as a named instance. Workers
	// launched then keep that config across upgrades, so drop it on load.
	delete(launch.TargetEnvironment, InstanceEnv)
	return launch, nil
}

func (c *WorkerLaunchConfig) Write(path string) error {
	parent := filepath.Dir(path)
	if err := os.MkdirAll(parent, 0o777); err != nil {
		return err
	}
	// serde_json::to_vec_pretty: two-space indent, no HTML escapes.
	var buffer bytes.Buffer
	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")
	if err := encoder.Encode(c); err != nil {
		return err
	}
	body := bytes.TrimSuffix(buffer.Bytes(), []byte("\n"))
	if err := os.WriteFile(path, body, 0o666); err != nil {
		return err
	}
	return os.Chmod(path, 0o600)
}

// WorkerExecutableDigest is the content address of a worker executable.
//
// One definition, because both sides of the upgrade decision compare it: the
// worker reports the digest of the file serving it, and the controller
// computes the digest of the file it would install. The digest and build
// stamp share a streaming read and are memoized by executable identity on
// Unix.
func WorkerExecutableDigest(path string) (string, error) {
	return executableDigest(path)
}

// RunningExecutableDigest is the content address of the executable running
// this process; ok is false when it cannot be read.
//
// Read through `/proc/self/exe` on Linux: a worker whose file was replaced
// under it - which is exactly what an upgrade does - still has its own image
// there, while the resolved path no longer names it.
func RunningExecutableDigest() (string, bool) {
	var path string
	if runtime.GOOS == "linux" {
		path = "/proc/self/exe"
	} else {
		executable, err := os.Executable()
		if err != nil {
			slog.Warn("could not resolve this executable to report its build", "error", err)
			return "", false
		}
		path = executable
	}
	digest, err := WorkerExecutableDigest(path)
	if err != nil {
		slog.Warn("could not hash this executable to report its build", "error", err.Error())
		return "", false
	}
	return digest, true
}

// ProfileProbeSpec is private, prompt-free harness discovery. All paths
// belong to one probe.
type ProfileProbeSpec struct {
	Harness     HarnessKind       `json:"harness"`
	ProfileHome string            `json:"profile_home"`
	Environment map[string]string `json:"environment"`
	// ExcludedEnvironment is the variables the probe removes from the harness
	// environment after it adds the login environment; see
	// WorkerLaunchConfig.ExcludedEnvironment.
	ExcludedEnvironment []string `json:"excluded_environment,omitempty"`
	Cwd                 string   `json:"cwd"`
	Model               *string  `json:"model"`
	// HarnessRuntime is where the probe's harness comes from: the same source
	// as a local session (LocalHarnessRuntime). Specs written by older
	// daemons omit it and always probed the managed installation.
	HarnessRuntime HarnessRuntimePolicy `json:"harness_runtime"`
}

func (s ProfileProbeSpec) MarshalJSON() ([]byte, error) {
	type plain ProfileProbeSpec
	value := plain(s)
	value.Environment = workerLaunchNonNilMap(value.Environment)
	return json.Marshal(value)
}

func (s *ProfileProbeSpec) UnmarshalJSON(data []byte) error {
	type plain ProfileProbeSpec
	value := plain{HarnessRuntime: managedHarnessRuntime()}
	if err := json.Unmarshal(data, &value); err != nil {
		return err
	}
	*s = ProfileProbeSpec(value)
	return nil
}

func managedHarnessRuntime() HarnessRuntimePolicy { return HarnessRuntimePolicyManaged }

type ProfileConfig struct {
	Model      *string               `json:"model"`
	Models     []SessionConfigChoice `json:"models"`
	Efforts    []SessionConfigChoice `json:"efforts"`
	ObservedAt int64                 `json:"observed_at"`
}

func (c ProfileConfig) MarshalJSON() ([]byte, error) {
	type plain ProfileConfig
	value := plain(c)
	value.Models = workerLaunchNonNilSlice(value.Models)
	value.Efforts = workerLaunchNonNilSlice(value.Efforts)
	return json.Marshal(value)
}

// RestrictedTo is the catalogue as Mjolnir offers it under a profile's
// ModelOffer: the listed models outside excluded scopes, plus the profile's
// current model, which stays so a session never runs a model its picker
// hides. An empty list with no excluded scope offers everything.
func (c ProfileConfig) RestrictedTo(offer *ModelOffer) ProfileConfig {
	if offer.IsUnrestricted() {
		return c
	}
	kept := make([]SessionConfigChoice, 0, len(c.Models))
	for _, choice := range c.Models {
		if offer.Allows(choice.Value) || (c.Model != nil && *c.Model == choice.Value) {
			kept = append(kept, choice)
		}
	}
	c.Models = kept
	return c
}

// OffersReviewTools reports whether a session with this review trigger on
// this harness is offered the mj-review tools. Launch and the daemon's
// completion checks share this one rule.
func OffersReviewTools(trigger ReviewTrigger, harness HarnessKind) bool {
	return trigger == ReviewTriggerPush && harness.SupportsReviewGate()
}

// workerLaunchDecodeStrict decodes data into v rejecting unknown fields, as
// serde's deny_unknown_fields does, and requires each named field to be
// present.
func workerLaunchDecodeStrict(data []byte, v any, required ...string) error {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(v); err != nil {
		return err
	}
	if len(required) == 0 {
		return nil
	}
	var fields map[string]json.RawMessage
	if err := json.Unmarshal(data, &fields); err != nil {
		return err
	}
	for _, name := range required {
		if _, ok := fields[name]; !ok {
			return fmt.Errorf("missing field `%s`", name)
		}
	}
	return nil
}

// workerLaunchNonNilSlice keeps a nil slice serializing as `[]`, as an empty
// Rust Vec does.
func workerLaunchNonNilSlice[T any](values []T) []T {
	if values == nil {
		return []T{}
	}
	return values
}

// workerLaunchNonNilMap keeps a nil map serializing as `{}`, as an empty Rust
// BTreeMap does.
func workerLaunchNonNilMap[K comparable, V any](values map[K]V) map[K]V {
	if values == nil {
		return map[K]V{}
	}
	return values
}
