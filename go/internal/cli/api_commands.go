// Port of mj-cli/src/api_commands.rs.
//
// The one-shot subcommands an orchestrating agent drives a session with.
//
// Each command is a thin call on apiClient: it builds the request struct the
// API exports, prints either the raw JSON response (`--json`) or a short
// human-readable summary, and returns. Nothing here interprets a session
// beyond formatting; the rules live behind the API.

package cli

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/BrokkAi/mjolnir/go/internal/sessionwiki"
	"github.com/charmbracelet/x/term"
	"github.com/spf13/cobra"
)

// sessionIDHelp is the help of every `--session` that names one session.
const sessionIDHelp = "Session id, as `mj sessions` lists it"

// jsonHelp is the help of most `--json` flags.
const jsonHelp = "Print the response as JSON instead of text"

// eventsArgs are the arguments of `mj events`.
type eventsArgs struct {
	// session restricts the stream to this session.
	session *string
	// workspaceID restricts the stream to this workspace ID.
	workspaceID *string
	// afterSeq replays events after this sequence; omit to follow new events
	// only.
	afterSeq  *uint64
	workspace workspaceName
}

// bind declares the events options on cmd.
func (args *eventsArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Restrict the stream to this session", storeOptional(&args.session))
	meta.option("workspace_id", "Restrict the stream to this workspace ID", storeOptional(&args.workspaceID))
	meta.option("after_seq", "Replay events after this sequence; omit to follow new events only", storeU64(&args.afterSeq, 0))
	args.workspace.bind(cmd)
	leaf(cmd, run)
}

// events follows durable session events as line-delimited JSON, following
// the daemon across an upgrade handoff from the cursor its stream reached.
func events(ctx context.Context, args eventsArgs, requestedWorkspace *string) error {
	workspaceID := args.workspaceID
	if workspaceID == nil && requestedWorkspace != nil {
		id, err := resolveStoreWorkspace(ctx, requestedWorkspace)
		if err != nil {
			return err
		}
		workspaceID = &id
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	filter := core.ApiEventFilter{SessionID: args.session, WorkspaceID: workspaceID}
	lastSeq := args.afterSeq
	resumeFrom := func() uint64 {
		if lastSeq == nil {
			return 0
		}
		return *lastSeq
	}
	interrupted, stop := signal.NotifyContext(ctx, os.Interrupt)
	defer stop()
	for {
		response, err := api.events(interrupted, &filter, lastSeq)
		if err != nil {
			if interrupted.Err() != nil && ctx.Err() == nil {
				return nil
			}
			// Reached the old daemon again while it exits: ask once more.
			if isDaemonHandoff(err) {
				if api, err = followHandoff(ctx, eventsContinuation(&filter, lastSeq)); err != nil {
					return err
				}
				continue
			}
			return err
		}
		resume, err := func() (uint64, error) {
			defer response.Body.Close()
			var decoder eventDecoder
			buffer := make([]byte, 64*1024)
			for {
				read, readErr := response.Body.Read(buffer)
				if read > 0 {
					decoded, err := decoder.push(buffer[:read])
					if err != nil {
						return 0, fmt.Errorf("decode event stream; resume with --after-seq %d: %w", resumeFrom(), err)
					}
					for _, event := range decoded {
						line, err := core.SerdeJSON(event)
						if err != nil {
							return 0, err
						}
						if _, err := os.Stdout.Write(append(line, '\n')); err != nil {
							return 0, err
						}
						seq := event.Seq
						lastSeq = &seq
					}
					if cursor, ok := decoder.handoff(); ok {
						return cursor, nil
					}
				}
				if readErr == nil {
					continue
				}
				if interrupted.Err() != nil && ctx.Err() == nil {
					return 0, errInterrupted
				}
				if errors.Is(readErr, io.EOF) {
					return 0, fmt.Errorf("event stream ended; resume with --after-seq %d", resumeFrom())
				}
				return 0, fmt.Errorf("event stream interrupted; resume with --after-seq %d: %w", resumeFrom(), readErr)
			}
		}()
		if errors.Is(err, errInterrupted) {
			return nil
		}
		if err != nil {
			return err
		}
		// The daemon is being replaced. It named the cursor its stream
		// reached, so the next daemon's stream continues from there and no
		// event is lost or printed twice.
		fmt.Fprintln(os.Stderr, "The Mjolnir daemon is being replaced by an upgrade; following the new daemon.")
		lastSeq = &resume
		if api, err = followHandoff(ctx, eventsContinuation(&filter, lastSeq)); err != nil {
			return err
		}
	}
}

// errInterrupted ends a stream the person stopped with Ctrl-C.
var errInterrupted = errors.New("interrupted")

// newArgs are the arguments of `mj new`.
type newArgs struct {
	// profile is the profile the session runs its harness from. Omit it to
	// use the saved default saved by an interactive launch. This command
	// does not change it.
	profile *string
	// target is the target template the session is provisioned on. Omit it
	// to use the saved default, as for `--profile`, without changing it.
	target *string
	// bundle is an existing bundle to run. Bare targets use
	// `--project-directory` instead.
	bundle *string
	// projectDirectory is an existing directory on the bare target, or a
	// checkout on the controller host for discovering an isolated project.
	projectDirectory *string
	// at starts the workspace checked out at this full commit ID in the
	// bundle's primary repository (`--at SHA`).
	at *string
	// noLaterHistory, with --at, gives the workspace the full history up to
	// that commit and --base, and nothing later: no later commits, branches,
	// tags, or origin remote. --base must then be a full commit ID. The
	// session cannot be resumed or moved. Requires at.
	noLaterHistory bool
	// branch, with --at, is the new branch to create there; otherwise the
	// existing branch to check out.
	branch *string
	// base records this Git revision as the diff base, when it should not be
	// --at (`--base REV`).
	base *string
	// workspaceID is the workspace id to create the session in.
	// `--workspace NAME` names the same workspace by name.
	workspaceID *string
	// title is shown in session lists. Defaults to the project and profile.
	title *string
	// model load balances across usable profiles for this model and
	// optional effort. Conflicts with profile.
	model *string
	// effort is the harness reasoning effort to select before the first
	// prompt.
	effort *string
	// subagents is the delegation policy: native, mjolnir, or none (no
	// delegation tools). Defaults to mjolnir, with a notice when unavailable
	// and native or none is used.
	subagents *string
	// oracle is oracle availability for this session: enabled or disabled.
	oracle *string
	// orchestrators enables orchestrators (requires Mjolnir delegation).
	orchestrators bool
	// subagentStraightforward overrides the configured model for
	// straightforward Mjolnir sub-agents.
	subagentStraightforward *string
	// subagentStraightforwardEffort overrides straightforward sub-agent
	// effort (requires --subagent-straightforward).
	subagentStraightforwardEffort *string
	// subagentSubtle overrides the configured model for subtle Mjolnir
	// sub-agents.
	subagentSubtle *string
	// subagentSubtleEffort overrides subtle sub-agent effort (requires
	// --subagent-subtle).
	subagentSubtleEffort *string
	// subagentOracle is the oracle model for this session, independent of
	// delegation.
	subagentOracle *string
	// subagentOracleEffort is the oracle effort; requires --subagent-oracle.
	subagentOracleEffort *string
	// subagentModel is retired (hidden); use the four per-type sub-agent
	// flags.
	subagentModel *string
	// subagentEffort is retired (hidden); use the four per-type sub-agent
	// flags.
	subagentEffort *string
	// reviewers is the reviewer model and optional effort
	// (`--reviewer MODEL[:EFFORT]`); the final colon suffix is effort only
	// when a known effort (minimal, low, medium, high, xhigh, max); a
	// trailing colon escapes it. Repeat up to three times.
	reviewers []string
	// review is whether this session is reviewed before push (`push`) or not
	// (`off`), `--review off|push`. Omitted follows `[review]`: review runs
	// when reviewers are configured.
	review *string
	// noProjectMemory starts this session with no project memory: nothing
	// from the workspace's project memory is loaded into the harness, and
	// the harness is offered no memory directory to write to. Sub-agents of
	// the session get none either. Without this flag the session loads the
	// workspace's project memory.
	noProjectMemory bool
	// noGitHubEvents excludes this session from GitHub watcher delivery
	// targets. Sub-agents inherit the choice.
	noGitHubEvents bool
	// noGitHubCredentials withholds GitHub credentials from this session and
	// its children.
	noGitHubCredentials bool
	// ignoreNotifications ignores notifications for this session.
	ignoreNotifications bool
	// cpus is the container CPU limit. Omitted takes the target's default
	// size: the size last chosen for that host, else 4 CPUs, capped at the
	// host's. At least 1.
	cpus *uint64
	// memoryGiB is the container memory limit in GiB (`--memory-gib GIB`).
	// Omitted takes the target's default size: the size last chosen for that
	// host, else 8 GiB, capped at the host's. At least 1.
	memoryGiB *uint64
	// prompt is the first prompt (positional). `-` reads it from standard
	// input.
	prompt *string
	// promptFile reads the first prompt from this file instead.
	promptFile *string
	// json prints the response as JSON instead of text.
	json      bool
	workspace workspaceName
}

// bind declares the new options on cmd.
func (args *newArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("profile", "Profile the session runs its harness from. Omit it to use the saved default saved by an interactive launch. This command does not change it", storeOptional(&args.profile))
	meta.option("target", "Target template the session is provisioned on. Omit it to use the saved default, as for `--profile`, without changing it", storeOptional(&args.target))
	meta.option("bundle", "Existing bundle to run. Bare targets use `--project-directory` instead", storeOptional(&args.bundle))
	meta.option("project_directory", "Existing directory on the bare target, or a checkout on the controller host for discovering an isolated project", storeOptional(&args.projectDirectory))
	meta.option("at", "Start the workspace checked out at this full commit ID in the bundle's primary repository", storeOptional(&args.at), withValueName("SHA"))
	meta.flag(&args.noLaterHistory, "no_later_history", "With --at, give the workspace the full history up to that commit and --base, and nothing later: no later commits, branches, tags, or origin remote. --base must then be a full commit ID. The session cannot be resumed or moved", requires("at"))
	meta.option("branch", "With --at, the new branch to create there; otherwise the existing branch to check out", storeOptional(&args.branch))
	meta.option("base", "Record this Git revision as the diff base, when it should not be --at", storeOptional(&args.base), withValueName("REV"))
	meta.option("workspace_id", "Workspace id to create the session in. `--workspace NAME` names the same workspace by name", storeOptional(&args.workspaceID))
	meta.option("title", "Title shown in session lists. Defaults to the project and profile", storeOptional(&args.title))
	meta.option("model", "Load balance across usable profiles for this model and optional effort", storeOptional(&args.model), conflictsWith("profile"))
	meta.option("effort", "Harness reasoning effort to select before the first prompt", storeOptional(&args.effort))
	meta.option("subagents", "Delegation policy: native, mjolnir, or none (no delegation tools). Defaults to mjolnir, with a notice when unavailable and native or none is used", storeOptional(&args.subagents))
	meta.option("oracle", "Oracle availability for this session", storeOptional(&args.oracle), possibleValues("enabled", "disabled"))
	meta.flag(&args.orchestrators, "orchestrators", "Enable orchestrators (requires Mjolnir delegation)")
	meta.option("subagent_straightforward", "Override the configured model for straightforward Mjolnir sub-agents", storeOptional(&args.subagentStraightforward))
	meta.option("subagent_straightforward_effort", "Override straightforward sub-agent effort (requires --subagent-straightforward)", storeOptional(&args.subagentStraightforwardEffort))
	meta.option("subagent_subtle", "Override the configured model for subtle Mjolnir sub-agents", storeOptional(&args.subagentSubtle))
	meta.option("subagent_subtle_effort", "Override subtle sub-agent effort (requires --subagent-subtle)", storeOptional(&args.subagentSubtleEffort))
	meta.option("subagent_oracle", "Oracle model for this session, independent of delegation", storeOptional(&args.subagentOracle))
	meta.option("subagent_oracle_effort", "Oracle effort; requires --subagent-oracle", storeOptional(&args.subagentOracleEffort), requires("subagent_oracle"))
	meta.option("subagent_model", "Retired; use the four per-type sub-agent flags", storeOptional(&args.subagentModel), hidden())
	meta.option("subagent_effort", "Retired; use the four per-type sub-agent flags", storeOptional(&args.subagentEffort), hidden())
	meta.option("reviewers", "Reviewer model and optional effort; final colon suffix is effort only when a known effort (minimal, low, medium, high, xhigh, max); trailing colon escapes it. Repeat up to three times", appendString(&args.reviewers), withLong("reviewer"), withValueName("MODEL[:EFFORT]"), multiple())
	meta.option("review", "Whether this session is reviewed before push (`push`) or not (`off`). Omitted follows `[review]`: review runs when reviewers are configured", storeOptional(&args.review), withValueName("off|push"))
	meta.flag(&args.noProjectMemory, "no_project_memory", "Start this session with no project memory: nothing from the workspace's project memory is loaded into the harness, and the harness is offered no memory directory to write to. Sub-agents of the session get none either. Without this flag the session loads the workspace's project memory")
	meta.flag(&args.noGitHubEvents, "no_github_events", "Exclude this session from GitHub watcher delivery targets. Sub-agents inherit the choice")
	meta.flag(&args.noGitHubCredentials, "no_github_credentials", "Withhold GitHub credentials from this session and its children")
	meta.flag(&args.ignoreNotifications, "ignore_notifications", "Ignore notifications for this session")
	meta.option("cpus", "Container CPU limit. Omitted takes the target's default size: the size last chosen for that host, else 4 CPUs, capped at the host's", storeU64(&args.cpus, 1))
	meta.option("memory_gib", "Container memory limit in GiB. Omitted takes the target's default size: the size last chosen for that host, else 8 GiB, capped at the host's", storeU64(&args.memoryGiB, 1), withValueName("GIB"))
	meta.positional("prompt", "The first prompt. `-` reads it from standard input", storeOptional(&args.prompt))
	meta.option("prompt_file", "Read the first prompt from this file instead", storeOptional(&args.promptFile))
	meta.flag(&args.json, "json", jsonHelp)
	args.workspace.bind(cmd)
	leaf(cmd, run)
}

// promptArgs are the arguments of `mj prompt`.
type promptArgs struct {
	// commandID is a stable producer-chosen identity; repeat it after an
	// ambiguous reply.
	commandID *string
	// session is the session id, as `mj sessions` lists it.
	session string
	// text is the prompt text (positional). `-` reads it from standard
	// input.
	text *string
	// promptFile reads the prompt from this file instead.
	promptFile *string
	// wait is retired (hidden); `mj prompt` always waits. Accepted so
	// scripts written for the retired flag keep working.
	wait bool
	// timeout is the seconds to wait for the turn to end before answering
	// `timeout`.
	timeout *uint64
	// returnOnInput returns when the harness asks for structured input.
	returnOnInput bool
	// json prints the turn's outcome as JSON instead of text.
	json bool
}

// bind declares the prompt options on cmd.
func (args *promptArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("command_id", "Stable producer-chosen identity; repeat it after an ambiguous reply", storeOptional(&args.commandID))
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.positional("text", "The prompt text. `-` reads it from standard input", storeOptional(&args.text))
	meta.option("prompt_file", "Read the prompt from this file instead", storeOptional(&args.promptFile))
	meta.flag(&args.wait, "wait", "Retired; `mj prompt` always waits", hidden())
	meta.option("timeout", "Seconds to wait for the turn to end before answering `timeout`", storeU64(&args.timeout, 0))
	meta.flag(&args.returnOnInput, "return_on_input", "Return when the harness asks for structured input")
	meta.flag(&args.json, "json", "Print the turn's outcome as JSON instead of text")
	leaf(cmd, run)
}

// eventArgs are the arguments of `mj event`.
type eventArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// key is the producer idempotency key. Omit it to generate a unique key.
	key *string
	// wake starts a turn if the session is idle when the worker receives
	// the event.
	wake bool
	// text is the event text (positional) shown to the session as untrusted
	// external content.
	text string
}

// bind declares the event options on cmd.
func (args *eventArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("key", "Producer idempotency key. Omit it to generate a unique key", storeOptional(&args.key))
	meta.flag(&args.wake, "wake", "Start a turn if the session is idle when the worker receives the event")
	meta.positional("text", "Event text shown to the session as untrusted external content", storeString(&args.text), required())
	leaf(cmd, run)
}

// messageArgs are the arguments of `mj message`.
type messageArgs struct {
	// session is the full session ID or unique case-insensitive hexadecimal
	// prefix of at least 8 characters.
	session string
	// commandID is a stable producer-chosen identity; repeat it after an
	// ambiguous reply.
	commandID *string
	// text is the message text (positional). `-` reads it from standard
	// input.
	text *string
	// promptFile reads the message from this file instead.
	promptFile *string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the message options on cmd.
func (args *messageArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Full session ID or unique case-insensitive hexadecimal prefix of at least 8 characters", storeString(&args.session), required())
	meta.option("command_id", "Stable producer-chosen identity; repeat it after an ambiguous reply", storeOptional(&args.commandID))
	meta.positional("text", "Message text. `-` reads it from standard input", storeOptional(&args.text))
	meta.option("prompt_file", "Read the message from this file instead", storeOptional(&args.promptFile))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// callingSession is the session this process runs inside, which Mjolnir
// names in `MJ_SESSION_ID` for every harness session; nil in the user's own
// shell.
func callingSession() (*string, error) {
	value, ok := os.LookupEnv("MJ_SESSION_ID")
	if !ok {
		return nil, nil
	}
	if !utf8.ValidString(value) {
		return nil, errors.New("MJ_SESSION_ID is not valid Unicode")
	}
	return &value, nil
}

// message sends a message and returns. From the user's own shell the daemon
// submits it as the user's prompt, steered into a running turn or queued;
// from inside a session it is that session's labelled message to another
// session or to one of its own sub-agents.
func message(ctx context.Context, args messageArgs) error {
	sender, err := callingSession()
	if err != nil {
		return err
	}
	text, err := readPrompt(args.text, args.promptFile)
	if err != nil {
		return err
	}
	if text == nil {
		return errors.New("pass the message text, --prompt-file, or `-` to read standard input")
	}
	fromTheUser := sender == nil
	requestID := ""
	if args.commandID != nil {
		requestID = *args.commandID
	} else if requestID, err = core.NewSessionID(); err != nil {
		return err
	}
	request := server.SessionMessageRequest{RequestID: requestID, Text: *text, SenderSessionID: sender}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	response, err := api.message(ctx, args.session, &request)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(response)
	}
	switch {
	case response.TurnID != nil && fromTheUser:
		fmt.Printf("turn %d\n", *response.TurnID)
	case response.TurnID != nil:
		fmt.Printf("message queued for session %s as turn %d\n", response.SessionID, *response.TurnID)
	default:
		fmt.Printf("message queued for session %s\n", response.SessionID)
	}
	return nil
}

// event queues an external event in a session's durable mailbox.
func event(ctx context.Context, args eventArgs) error {
	key := ""
	if args.key != nil {
		key = *args.key
	} else {
		id, err := core.NewSessionID()
		if err != nil {
			return err
		}
		key = "cli:" + id
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	response, err := api.enqueueEvent(ctx, args.session, &server.MailboxEventRequest{Key: key, Text: args.text, Wake: args.wake})
	if err != nil {
		return err
	}
	queued := "already queued"
	if response.Inserted {
		queued = "queued"
	}
	fmt.Printf("%s event %s for session %s\n", queued, response.Key, args.session)
	return nil
}

// waitArgs are the arguments of `mj wait`.
type waitArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// turn waits until this turn, as `mj new` or `mj message` printed it, or
	// a later one, has ended, and says how. The wait does not follow what
	// the session does after the turn (a turn the agent starts on its own,
	// background work, an automatic continuation); the answer names that as
	// `next`. It describes the latest turn that ended (a session keeps only
	// that turn's outcome and reply), and `--json` reports it as `turn_id`
	// beside `requested_turn_id`. Omit it to wait until the session settles:
	// idle, with nothing queued, including after turns the agent starts on
	// its own.
	turn *uint64
	// timeout is the seconds to wait before answering `timeout`.
	timeout *uint64
	// returnOnInput returns when the harness asks for structured input.
	returnOnInput bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the wait options on cmd.
func (args *waitArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("turn", "Wait until this turn, as `mj new` or `mj message` printed it, or a later one, has ended, and say how. The wait does not follow what the session does after the turn (a turn the agent starts on its own, background work, an automatic continuation); the answer names that as `next`. It describes the latest turn that ended (a session keeps only that turn's outcome and reply), and `--json` reports it as `turn_id` beside `requested_turn_id`. Omit it to wait until the session settles: idle, with nothing queued, including after turns the agent starts on its own", storeU64(&args.turn, 0))
	meta.option("timeout", "Seconds to wait before answering `timeout`", storeU64(&args.timeout, 0))
	meta.flag(&args.returnOnInput, "return_on_input", "Return when the harness asks for structured input")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// transcriptArgs are the arguments of `mj transcript`.
type transcriptArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// afterSeq starts after this sequence; use next_after_seq to resume
	// after a page.
	afterSeq *uint64
	// limit is the target number of matching entries in a page; sequence
	// ties stay together.
	limit *int
	// role returns only items with this role; repeat to select several
	// roles. Parsed by parseTranscriptRole.
	role []core.TranscriptRole
	// finishedOnly returns finished agent messages only, with a cursor held
	// before open messages.
	finishedOnly bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the transcript options on cmd.
func (args *transcriptArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("after_seq", "Start after this sequence; use next_after_seq to resume after a page", storeU64(&args.afterSeq, 0))
	meta.option("limit", "Target number of matching entries in a page; sequence ties stay together", storeUsize(&args.limit))
	meta.option("role", "Return only items with this role; repeat to select several roles", func(raw string) error {
		role, err := parseTranscriptRole(raw)
		if err != nil {
			return err
		}
		args.role = append(args.role, role)
		return nil
	}, multiple())
	meta.flag(&args.finishedOnly, "finished_only", "Return finished agent messages only, with a cursor held before open messages")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// parseTranscriptRole is the value parser of `--role`.
func parseTranscriptRole(value string) (core.TranscriptRole, error) {
	encoded, err := json.Marshal(value)
	if err != nil {
		return "", err
	}
	var role core.TranscriptRole
	if err := json.Unmarshal(encoded, &role); err != nil || !slices.Contains(transcriptRoles, role) {
		return "", fmt.Errorf("unknown transcript role %s; expected user, agent, message, thought, tool, terminal, plan, plan_proposal, or system", core.RustDebugString(value))
	}
	return role, nil
}

// transcriptRoles is every role serde accepts for a TranscriptRole.
var transcriptRoles = []core.TranscriptRole{
	core.TranscriptRoleUser,
	core.TranscriptRoleAgent,
	core.TranscriptRoleMessage,
	core.TranscriptRoleThought,
	core.TranscriptRoleTool,
	core.TranscriptRoleTerminal,
	core.TranscriptRolePlan,
	core.TranscriptRolePlanProposal,
	core.TranscriptRoleSystem,
}

// requestedTranscriptRoles is the roles a transcript page asks for: agent
// messages only for `--finished-only`, the named roles, or every role but
// tool and terminal.
func requestedTranscriptRoles(args *transcriptArgs) ([]core.TranscriptRole, error) {
	if args.finishedOnly {
		for _, role := range args.role {
			if role != core.TranscriptRoleAgent {
				return nil, errors.New("--finished-only can only be combined with --role agent")
			}
		}
		return []core.TranscriptRole{core.TranscriptRoleAgent}, nil
	}
	if len(args.role) == 0 {
		return []core.TranscriptRole{
			core.TranscriptRoleUser,
			core.TranscriptRoleAgent,
			core.TranscriptRoleMessage,
			core.TranscriptRoleThought,
			core.TranscriptRolePlan,
			core.TranscriptRolePlanProposal,
			core.TranscriptRoleSystem,
		}, nil
	}
	return append([]core.TranscriptRole(nil), args.role...), nil
}

// usageArgs are the arguments of `mj usage`.
type usageArgs struct {
	// session reads one session, including accounting retained after
	// cleanup. Required unless parent; conflicts with parent.
	session *string
	// parent reads this session and every descendant, including cleaned-up
	// children. Required unless session; conflicts with session.
	parent *string
	// afterSeq returns entries after this sequence number (one session
	// only). Conflicts with parent.
	afterSeq *uint64
	// limit is the most entries to return in one page (one session only).
	// Conflicts with parent.
	limit *int
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the usage options on cmd.
func (args *usageArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Read one session, including accounting retained after cleanup", storeOptional(&args.session), requiredUnlessPresent("parent"), conflictsWith("parent"))
	meta.option("parent", "Read this session and every descendant, including cleaned-up children", storeOptional(&args.parent), requiredUnlessPresent("session"), conflictsWith("session"))
	meta.option("after_seq", "Return entries after this sequence number (one session only)", storeU64(&args.afterSeq, 0), conflictsWith("parent"))
	meta.option("limit", "Most entries to return in one page (one session only)", storeUsize(&args.limit), conflictsWith("parent"))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// usage reads recorded token usage and coverage.
func usage(ctx context.Context, args usageArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if args.parent != nil {
		tree, err := api.usageTree(ctx, *args.parent)
		if err != nil {
			return err
		}
		if args.json {
			return printJSON(tree)
		}
		printLines(usageTreeLines(&tree))
		return nil
	}
	if args.session == nil {
		return errors.New("usage requires --session or --parent")
	}
	page, err := api.usage(ctx, *args.session, args.afterSeq, args.limit)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(page)
	}
	printLines(usageLines(&page))
	return nil
}

// sortedKeys is a map's keys in order, as a BTreeMap iterates.
func sortedKeys[V any](m map[string]V) []string {
	return slices.Sorted(maps.Keys(m))
}

// modelUsageLines is the per-model and effort totals, one counter per line.
func modelUsageLines(groups []core.UsageModelTotal) []string {
	var lines []string
	for _, group := range groups {
		model, effort := "unknown", "unknown"
		if group.Selection.Model != nil {
			model = *group.Selection.Model
		}
		if group.Selection.Effort != nil {
			effort = *group.Selection.Effort
		}
		lines = append(lines, fmt.Sprintf("model %s; effort %s:", model, effort))
		for _, counter := range sortedKeys(group.Totals) {
			total := group.Totals[counter]
			lines = append(lines, fmt.Sprintf("  %s  %d  (%d turns)", counter, total.Tokens, total.ReportedTurns))
		}
	}
	return lines
}

// turnCostUSD is a priced turn cost in USD ticks as text.
func turnCostUSD(ticks uint64) string {
	return core.ProviderTurnCostFromUsdTicks(ticks, false).Usd
}

// usageTreeLines is what `mj usage --parent` prints without `--json`.
func usageTreeLines(tree *core.UsageTree) []string {
	coverage := tree.Summary.Coverage
	lines := []string{
		fmt.Sprintf("usage tree %s: %d sessions", tree.ParentSessionID, len(tree.Sessions)),
		fmt.Sprintf("totals from the %d of %d turns that reported a whole turn; %d started turns without a completion report",
			coverage.FullTurnReports, coverage.RecordedTurns, coverage.UnfinishedTurns),
	}
	lines = append(lines, modelUsageLines(tree.Summary.ByModel)...)
	lines = append(lines,
		fmt.Sprintf("not in totals: %d last-request, %d unknown-scope, %d missing reports",
			coverage.LastRequestReports, coverage.UnspecifiedReports, coverage.MissingReports),
		fmt.Sprintf("priced turn cost: %s USD (%d provider, %d estimated, %d missing costs)",
			turnCostUSD(tree.Summary.TurnCostUsdTicks), coverage.ProviderCostTurns, coverage.EstimatedCostTurns, coverage.MissingCostTurns),
	)
	for _, session := range tree.Sessions {
		task, parent := "root", "none"
		if session.TaskName != nil {
			task = *session.TaskName
		}
		if session.ParentSessionID != nil {
			parent = *session.ParentSessionID
		}
		retained := "accounting retained after cleanup"
		if session.OperationalSessionPresent {
			retained = "retained session"
		}
		lines = append(lines, fmt.Sprintf("session %s: task %s; parent %s; %s", session.SessionID, task, parent, retained))
		if cost := session.Summary.ProviderSessionCost; cost != nil {
			lines = append(lines, fmt.Sprintf("  provider session cost: %.4f %s", cost.Amount, cost.Currency))
		}
		costCoverage := session.Summary.Coverage
		lines = append(lines, fmt.Sprintf("  priced turn cost: %s USD (%d provider, %d estimated, %d missing costs)",
			turnCostUSD(session.Summary.TurnCostUsdTicks), costCoverage.ProviderCostTurns, costCoverage.EstimatedCostTurns, costCoverage.MissingCostTurns))
	}
	return lines
}

// usageLines is what `mj usage` prints without `--json`.
//
// The coverage is printed with the totals, never apart from them: a harness
// that reports only its last request, or nothing, would otherwise make a
// partial count read as the whole session's.
func usageLines(page *core.UsagePage) []string {
	coverage := page.Coverage
	lines := []string{fmt.Sprintf("totals from the %d of %d turns that reported a whole turn:", coverage.FullTurnReports, coverage.RecordedTurns)}
	if len(page.Totals) == 0 {
		lines = append(lines, "  no counters")
	}
	for _, counter := range sortedKeys(page.Totals) {
		total := page.Totals[counter]
		lines = append(lines, fmt.Sprintf("  %s  %d  (%d turns)", counter, total.Tokens, total.ReportedTurns))
	}
	var leftOut []string
	for _, entry := range []struct {
		count uint64
		what  string
	}{
		{coverage.LastRequestReports, "reported only their last request"},
		{coverage.UnspecifiedReports, "reported an unknown scope"},
		{coverage.MissingReports, "reported nothing"},
	} {
		if entry.count > 0 {
			leftOut = append(leftOut, fmt.Sprintf("%d %s", entry.count, entry.what))
		}
	}
	if len(leftOut) > 0 {
		lines = append(lines, fmt.Sprintf("not in the totals: %s (turns)", strings.Join(leftOut, ", ")))
	}
	if cost := page.ProviderSessionCost; cost != nil {
		lines = append(lines, fmt.Sprintf("provider cost for the session so far: %.4f %s", cost.Amount, cost.Currency))
	}
	lines = append(lines, fmt.Sprintf("priced turn cost: %s USD (%d provider, %d estimated, %d missing costs)",
		turnCostUSD(page.TurnCostUsdTicks), coverage.ProviderCostTurns, coverage.EstimatedCostTurns, coverage.MissingCostTurns))
	lines = append(lines, modelUsageLines(page.ByModel)...)
	if coverage.UnfinishedTurns > 0 {
		lines = append(lines, fmt.Sprintf("%d started turns without a completion report", coverage.UnfinishedTurns))
	}
	lines = append(lines, fmt.Sprintf("next after seq %d; latest seq %d", page.NextAfterSeq, page.LatestSeq))
	return lines
}

// putFileArgs are the arguments of `mj put-file`.
type putFileArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// path is the destination relative to the session workspace.
	path string
	// source is the source file (positional), or - for stdin.
	source string
	// overwrite replaces the file if it already exists.
	overwrite bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the put-file options on cmd.
func (args *putFileArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("path", "Destination relative to the session workspace", storeString(&args.path), required())
	meta.positional("source", "Source file, or - for stdin", storeString(&args.source), required())
	meta.flag(&args.overwrite, "overwrite", "Replace the file if it already exists")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// elicitationsArgs are the arguments of `mj elicitations`.
type elicitationsArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the elicitations options on cmd.
func (args *elicitationsArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// respondArgs are the arguments of `mj respond`.
type respondArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// elicitation is the id of the input request to answer, as
	// `mj elicitations` lists it.
	elicitation string
	// response is the JSON response (positional), or - for stdin.
	response *string
	// responseFile reads the JSON response from this file instead.
	responseFile *string
}

// bind declares the respond options on cmd.
func (args *respondArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("elicitation", "Id of the input request to answer, as `mj elicitations` lists it", storeString(&args.elicitation), required())
	meta.positional("response", "JSON response, or - for stdin", storeOptional(&args.response))
	meta.option("response_file", "Read the JSON response from this file instead", storeOptional(&args.responseFile))
	leaf(cmd, run)
}

// putFile atomically uploads a file, or standard input, to an idle session
// workspace.
func putFile(ctx context.Context, args putFileArgs) error {
	var body io.Reader
	var contentLength *uint64
	if args.source == "-" {
		body = os.Stdin
	} else {
		file, err := os.Open(args.source)
		if err != nil {
			return fmt.Errorf("open %s: %w", args.source, err)
		}
		defer file.Close()
		info, err := file.Stat()
		if err != nil {
			return fmt.Errorf("inspect %s: %w", args.source, err)
		}
		body = file
		if info.Mode().IsRegular() {
			length := uint64(info.Size())
			contentLength = &length
		}
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	written, err := api.putFile(ctx, args.session, args.path, body, contentLength, args.overwrite)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(written)
	}
	fmt.Printf("wrote %d bytes to %s\n", written.Bytes, written.Path)
	return nil
}

// elicitations prints a session's structured input requests as JSON.
func elicitations(ctx context.Context, args elicitationsArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	requests, err := api.elicitations(ctx, args.session)
	if err != nil {
		return err
	}
	return printJSON(requests)
}

// respond answers a structured input request.
func respond(ctx context.Context, args respondArgs) error {
	text, err := readPrompt(args.response, args.responseFile)
	if err != nil {
		return err
	}
	if text == nil {
		return errors.New("provide a JSON response or --response-file")
	}
	var response core.ElicitationResponse
	if err := json.Unmarshal([]byte(*text), &response); err != nil {
		return fmt.Errorf("parse elicitation response JSON: %w", err)
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if err := api.respondElicitation(ctx, args.session, args.elicitation, &response); err != nil {
		return err
	}
	fmt.Println("response accepted")
	return nil
}

// diffArgs are the arguments of `mj diff`.
type diffArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// base compares against this Git commit or revision instead of the
	// launch base.
	base *string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the diff options on cmd.
func (args *diffArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("base", "Compare against this Git commit or revision instead of the launch base", storeOptional(&args.base))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// fetchArgs are the arguments of `mj fetch`.
type fetchArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// path is the file or directory to fetch: relative to the directory the
	// session's agent runs in, or absolute on the session's target. It must
	// lie in the session's workspace, its sub-agent report directories, or
	// the target's temporary directory.
	path string
	// stdout writes the file, or the directory's zip archive, to standard
	// output instead of the temporary directory. Conflicts with json.
	stdout bool
	// ls lists the directory, or the file, with each entry's size and
	// modification time, on standard output. Symlinks are listed, not
	// followed.
	ls bool
	// json prints the result as JSON: the listing with `--ls`, otherwise
	// where the copy landed.
	json bool
}

// bind declares the fetch options on cmd.
func (args *fetchArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("path", "File or directory to fetch: relative to the directory the session's agent runs in, or absolute on the session's target. It must lie in the session's workspace, its sub-agent report directories, or the target's temporary directory", storeString(&args.path), required())
	meta.flag(&args.stdout, "stdout", "Write the file, or the directory's zip archive, to standard output instead of the temporary directory", conflictsWith("json"))
	meta.flag(&args.ls, "ls", "List the directory, or the file, with each entry's size and modification time, on standard output. Symlinks are listed, not followed")
	meta.flag(&args.json, "json", "Print the result as JSON: the listing with `--ls`, otherwise where the copy landed")
	leaf(cmd, run)
}

// exportArgs are the arguments of `mj export`.
type exportArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// kind is what to export: a patch, a pushed branch, or a git bundle.
	// Defaults to patch.
	kind exportKindArg
	// branch is the branch to push, required by `--kind branch`.
	branch *string
	// out writes the export here instead of standard output.
	out *string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the export options on cmd.
func (args *exportArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("kind", "What to export: a patch, a pushed branch, or a git bundle", func(raw string) error {
		args.kind = exportKindArg(raw)
		return nil
	}, possibleValues("patch", "branch", "bundle"), defaultValue("patch"))
	meta.option("branch", "Branch to push, required by `--kind branch`", storeOptional(&args.branch), requiredIfEq("kind", "branch"))
	meta.option("out", "Write the export here instead of standard output", storeOptional(&args.out))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// exportKindArg is `mj export --kind`.
type exportKindArg string

const (
	exportKindArgPatch  exportKindArg = "patch"
	exportKindArgBranch exportKindArg = "branch"
	exportKindArgBundle exportKindArg = "bundle"
)

// sessionsArgs are the arguments of `mj sessions`.
type sessionsArgs struct {
	// session shows one session, including how its last turn ended.
	session *string
	// all includes sub-agents as well as top-level sessions.
	all bool
	// target lists only sessions on this target, as `mj hosts` names its
	// targets. A fleet's instances are selected this way. Conflicts with
	// session.
	target *string
	// host lists only sessions on this machine, as `mj hosts` names its
	// hosts. Conflicts with session.
	host *string
	// json prints the response as JSON instead of text.
	json      bool
	workspace workspaceName
}

// bind declares the sessions options on cmd.
func (args *sessionsArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Show one session, including how its last turn ended", storeOptional(&args.session))
	meta.flag(&args.all, "all", "Include sub-agents as well as top-level sessions")
	meta.option("target", "List only sessions on this target, as `mj hosts` names its targets. A fleet's instances are selected this way", storeOptional(&args.target), conflictsWith("session"))
	meta.option("host", "List only sessions on this machine, as `mj hosts` names its hosts", storeOptional(&args.host), conflictsWith("session"))
	meta.flag(&args.json, "json", jsonHelp)
	args.workspace.bind(cmd)
	leaf(cmd, run)
}

// sessionArgs are the arguments of a command on one session.
type sessionArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the session options on cmd.
func (args *sessionArgs) bind(cmd *cobra.Command, run func() error) {
	args.declare(clapOf(cmd))
	leaf(cmd, run)
}

// declare declares the session options, flattened into a larger command.
func (args *sessionArgs) declare(meta *clapCommand) {
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.flag(&args.json, "json", jsonHelp)
}

// stopTaskArgs are the arguments of `mj stop-task`.
type stopTaskArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// taskID is the opaque task id from `mj sessions --session <id> --json`
	// (positional TASK_ID). Required unless all; conflicts with all.
	taskID *string
	// all stops every currently listed task that the worker can stop.
	all bool
	// json prints accepted task ids, skipped task ids and failures as JSON.
	json bool
}

// bind declares the stop-task options on cmd.
func (args *stopTaskArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.positional("task_id", "Opaque task id from `mj sessions --session <id> --json`", storeOptional(&args.taskID), requiredUnlessPresent("all"), conflictsWith("all"))
	meta.flag(&args.all, "all", "Stop every currently listed task that the worker can stop")
	meta.flag(&args.json, "json", "Print accepted task ids, skipped task ids and failures as JSON")
	leaf(cmd, run)
}

// stopTaskReport is what `mj stop-task` reports: the tasks stopped, the ones
// the worker cannot stop, and each failure. The JSON encoding must write
// every list, empty or not, as serde does.
type stopTaskReport struct {
	SessionID       string            `json:"session_id"`
	AcceptedTaskIDs []string          `json:"accepted_task_ids"`
	SkippedTaskIDs  []string          `json:"skipped_task_ids"`
	Failures        map[string]string `json:"failures"`
}

// stopTask stops a session's background task, or all tasks the worker can
// stop.
func stopTask(ctx context.Context, args stopTaskArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	report, err := stopTasks(ctx, api, &args)
	if err != nil {
		return err
	}
	if args.json {
		if err := printJSON(report); err != nil {
			return err
		}
	} else {
		for _, id := range report.AcceptedTaskIDs {
			fmt.Printf("stop accepted for background task %s in %s\n", id, report.SessionID)
		}
		for _, id := range report.SkippedTaskIDs {
			fmt.Printf("background task %s cannot be stopped by this worker; skipped\n", id)
		}
		for _, id := range sortedKeys(report.Failures) {
			fmt.Fprintf(os.Stderr, "background task %s: %s\n", id, report.Failures[id])
		}
		if len(report.AcceptedTaskIDs) == 0 && len(report.Failures) == 0 {
			fmt.Printf("no stoppable background tasks in %s\n", report.SessionID)
		}
	}
	if len(report.Failures) > 0 {
		return fmt.Errorf("could not stop %d background task(s)", len(report.Failures))
	}
	return nil
}

// stopTasks asks the daemon to stop each selected task, together, and
// reports each outcome.
func stopTasks(ctx context.Context, api *apiClient, args *stopTaskArgs) (stopTaskReport, error) {
	report := stopTaskReport{
		SessionID:       args.session,
		AcceptedTaskIDs: []string{},
		SkippedTaskIDs:  []string{},
		Failures:        map[string]string{},
	}
	var taskIDs []string
	if args.all {
		session, err := api.sessionIfKnown(ctx, args.session)
		if err != nil {
			return stopTaskReport{}, err
		}
		if session == nil {
			return stopTaskReport{}, fmt.Errorf("unknown session %s", args.session)
		}
		for _, task := range session.BackgroundTasks {
			if task.CanStop {
				taskIDs = append(taskIDs, task.ID)
			} else {
				report.SkippedTaskIDs = append(report.SkippedTaskIDs, task.ID)
			}
		}
	} else {
		if args.taskID == nil {
			return stopTaskReport{}, errors.New("name a task id or use --all")
		}
		taskIDs = []string{*args.taskID}
	}
	outcomes := make([]error, len(taskIDs))
	var stopping sync.WaitGroup
	for index, id := range taskIDs {
		stopping.Go(func() { outcomes[index] = api.stopBackgroundTask(ctx, args.session, id) })
	}
	stopping.Wait()
	for index, id := range taskIDs {
		if outcomes[index] == nil {
			report.AcceptedTaskIDs = append(report.AcceptedTaskIDs, id)
		} else {
			report.Failures[id] = outcomes[index].Error()
		}
	}
	return report, nil
}

// suspendArgs suspend a session, keeping only its Git state (branches, tags,
// stashes, commits, staged and unstaged changes to tracked files, and linked
// worktrees inside the checkout). Untracked files are lost unless you
// `git add` them first; Git-ignored files are never kept. Scratch in
// `$MJ_KEEP_DIR` travels with Move but is not saved when the session is
// suspended.
type suspendArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session *string
	// misplacedSession (positional SESSION, hidden) is taken only so the
	// command can explain itself. `mj suspend <id>` is a common mistake, and
	// every session command in this CLI names its session with `--session`,
	// so clap's "unexpected argument" would leave the person guessing which
	// option it wanted.
	misplacedSession *string
	// discardUntracked discards untracked files, nested repositories,
	// untracked stashes, and `.mj/keep/` contents.
	discardUntracked bool
	// abortOperation aborts any in-progress Git operation (merge, rebase,
	// cherry-pick, revert, am, bisect).
	abortOperation bool
	// withoutCheckpoint stops the session without taking a fresh checkpoint.
	withoutCheckpoint bool
	// yes confirms `--without-checkpoint` when no verified checkpoint exists
	// to resume from.
	yes bool
	// dryRun inspects what would be saved and what would be lost without
	// changing anything.
	dryRun bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the suspend options on cmd.
func (args *suspendArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeOptional(&args.session))
	meta.positional("misplaced_session", "Taken only so the command can explain itself. `mj suspend <id>` is a common mistake, and every session command in this CLI names its session with `--session`, so clap's \"unexpected argument\" would leave the person guessing which option it wanted", storeOptional(&args.misplacedSession), withValueName("SESSION"), hidden())
	meta.flag(&args.discardUntracked, "discard_untracked", "Discard untracked files, nested repositories, untracked stashes, and `.mj/keep/` contents")
	meta.flag(&args.abortOperation, "abort_operation", "Abort any in-progress Git operation (merge, rebase, cherry-pick, revert, am, bisect)")
	meta.flag(&args.withoutCheckpoint, "without_checkpoint", "Stop the session without taking a fresh checkpoint")
	meta.flag(&args.yes, "yes", "Confirm `--without-checkpoint` when no verified checkpoint exists to resume from")
	meta.flag(&args.dryRun, "dry_run", "Inspect what would be saved and what would be lost without changing anything")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// destroyArgs remove the session even if environment cleanup fails; report
// remaining resources in a warning.
type destroyArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the destroy options on cmd.
func (args *destroyArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// noticesArgs are the arguments of `mj notices`.
type noticesArgs struct {
	// session is the session id; notices remain readable after the session
	// is removed.
	session string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the notices options on cmd.
func (args *noticesArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Session id; notices remain readable after the session is removed", storeString(&args.session), required())
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// notices reads recent cleanup notices, including after a session was
// destroyed.
func notices(ctx context.Context, args noticesArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	notices, err := api.notices(ctx, args.session)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(notices)
	}
	if len(notices) == 0 {
		fmt.Printf("No cleanup notices are currently retained for %s. Notices can arrive while destruction runs; they are kept in the current daemon's recent notice history.\n", args.session)
	}
	for _, notice := range notices {
		fmt.Println(core.SanitizeTerminalText(notice.Text))
	}
	return nil
}

// resumeArgs are the arguments of `mj resume`. The group "resume-subject"
// requires one of session and wiki.
type resumeArgs struct {
	// session is the suspended, lost, or failed session to resume. It keeps
	// its identity, transcript, and work.
	session *string
	// wiki is the SessionWiki id of a session to continue, whoever ran it: a
	// Mjolnir session is resumed or restored, another tool's session is
	// imported and then resumed.
	wiki *string
	// profile is the profile to resume on. Defaults to the one the session
	// last ran.
	profile *string
	// target is the target template to provision. Defaults to the session's
	// own.
	target *string
	// workspaceID is the workspace to resume into. Defaults to the session's
	// own.
	workspaceID *string
	// queue is what to do with prompts queued when the session was
	// suspended.
	queue *resumeQueueArg
	// json prints the response as JSON instead of text.
	json      bool
	workspace workspaceName
}

// bind declares the resume options and the resume-subject group on cmd.
func (args *resumeArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Suspended, lost, or failed session to resume. It keeps its identity, transcript, and work", storeOptional(&args.session))
	meta.option("wiki", "SessionWiki id of a session to continue, whoever ran it: a Mjolnir session is resumed or restored, another tool's session is imported and then resumed", storeOptional(&args.wiki))
	meta.option("profile", "Profile to resume on. Defaults to the one the session last ran", storeOptional(&args.profile))
	meta.option("target", "Target template to provision. Defaults to the session's own", storeOptional(&args.target))
	meta.option("workspace_id", "Workspace to resume into. Defaults to the session's own", storeOptional(&args.workspaceID))
	meta.option("queue", "What to do with prompts queued when the session was suspended", func(raw string) error {
		queue := resumeQueueArg(raw)
		args.queue = &queue
		return nil
	}, possibleValues("start", "discard"))
	meta.flag(&args.json, "json", jsonHelp)
	args.workspace.bind(cmd)
	meta.group("resume-subject", true, false, "session", "wiki")
	leaf(cmd, run)
}

// resumeQueueArg is `mj resume --queue`.
type resumeQueueArg string

const (
	resumeQueueArgStart   resumeQueueArg = "start"
	resumeQueueArgDiscard resumeQueueArg = "discard"
)

// disposition is the resume queue disposition the choice stands for.
func (queue resumeQueueArg) disposition() core.ResumeQueueDisposition {
	if queue == resumeQueueArgStart {
		return core.ResumeQueueDispositionStart
	}
	return core.ResumeQueueDispositionDiscard
}

// apiInfoArgs are the arguments of `mj api-info`.
type apiInfoArgs struct {
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the api-info options on cmd.
func (args *apiInfoArgs) bind(cmd *cobra.Command, run func() error) {
	clapOf(cmd).flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// githubTokenArgs are the arguments of `mj github-token`. The group
// "github_token_target" requires exactly one of owner and repo.
type githubTokenArgs struct {
	// owner is the GitHub owner login whose installation should receive a
	// token.
	owner *string
	// repo is a repository in OWNER/NAME form. May be repeated to limit the
	// token to several repositories from one installation.
	repo []string
}

// bind declares the github-token options and their group on cmd.
func (args *githubTokenArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("owner", "GitHub owner login whose installation should receive a token", storeOptional(&args.owner))
	meta.option("repo", "Repository in OWNER/NAME form. May be repeated to limit the token to several repositories from one installation", appendString(&args.repo), multiple())
	meta.group("github_token_target", true, false, "owner", "repo")
	leaf(cmd, run)
}

// workspacesListArgs are the arguments of `mj workspaces list`.
type workspacesListArgs struct {
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the workspaces list options on cmd.
func (args *workspacesListArgs) bind(cmd *cobra.Command, run func() error) {
	clapOf(cmd).flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// workspaceCreateArgs are the arguments of `mj workspaces create`.
type workspaceCreateArgs struct {
	// name is the workspace name (positional), 1-64 characters. Naming one
	// that already exists selects it instead of failing, so this is safe to
	// run before every session.
	name string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the workspaces create options on cmd.
func (args *workspaceCreateArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.positional("name", "Workspace name, 1-64 characters. Naming one that already exists selects it instead of failing, so this is safe to run before every session", storeString(&args.name), required())
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// workspacesList lists the workspaces sessions can be created in.
func workspacesList(ctx context.Context, args workspacesListArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	list, err := api.workspaces(ctx)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(list)
	}
	idWidth := 2
	for _, workspace := range list.Workspaces {
		idWidth = max(idWidth, utf8.RuneCountInString(workspace.ID))
	}
	fmt.Printf("%s  SESSIONS  NAME\n", padRight("ID", idWidth))
	for _, workspace := range list.Workspaces {
		fmt.Printf("%s  %8d  %s\n", padRight(workspace.ID, idWidth), workspace.SessionCount, core.SanitizeTerminalText(workspace.Name))
	}
	return nil
}

// padRight pads text with spaces to width characters, as Rust's `{:width$}`
// pads a string.
func padRight(text string, width int) string {
	return text + strings.Repeat(" ", max(width-utf8.RuneCountInString(text), 0))
}

// workspacesCreate creates a workspace, or selects the one that already
// carries the name.
func workspacesCreate(ctx context.Context, args workspaceCreateArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	created, err := api.createWorkspace(ctx, args.name)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(created)
	}
	fmt.Printf("workspace %s  %s\n", created.Workspace.ID, core.SanitizeTerminalText(created.Workspace.Name))
	return nil
}

// newSubagentPolicy is the delegation policy `mj new`'s flags ask for, or
// nil to use the configured one.
func newSubagentPolicy(args *newArgs) (*core.SubagentPolicy, error) {
	return subagentPolicy(
		args.subagents,
		args.subagentStraightforward,
		args.subagentStraightforwardEffort,
		args.subagentSubtle,
		args.subagentSubtleEffort,
		args.subagentModel,
		args.subagentEffort,
		args.orchestrators,
	)
}

// oracleOverride is the oracle model and effort the flags name, or nil when
// they name none.
func oracleOverride(model, effort, enabled *string) (*core.SubagentChoice, error) {
	if effort != nil && model == nil {
		return nil, errors.New("--subagent-oracle-effort requires --subagent-oracle")
	}
	if effort != nil && *effort == "adaptive" {
		return nil, errors.New("adaptive oracle effort is not supported")
	}
	if model != nil && enabled != nil && *enabled == "disabled" {
		return nil, errors.New("--subagent-oracle cannot be used with --oracle disabled")
	}
	if model != nil && strings.TrimSpace(*model) == "" {
		return nil, errors.New("--subagent-oracle requires a model name")
	}
	if model == nil {
		return nil, nil
	}
	return &core.SubagentChoice{Model: *model, Effort: effort}, nil
}

// subagentPolicy is the delegation policy the flags shared by `mj new` and
// `mj move` ask for, or nil to keep the default.
func subagentPolicy(
	mode, straightforward, straightforwardEffort, subtle, subtleEffort, retiredModel, retiredEffort *string,
	orchestrators bool,
) (*core.SubagentPolicy, error) {
	const remedy = "Configure [subagents.straightforward] and optionally [subagents.subtle] in config.toml or TUI Settings -> Sub-agents"
	is := func(value *string, text string) bool { return value != nil && *value == text }
	if retiredModel != nil || retiredEffort != nil {
		return nil, errors.New("--subagent-model and --subagent-effort are retired; use --subagent-straightforward, --subagent-straightforward-effort, --subagent-subtle, and --subagent-subtle-effort")
	}
	if straightforwardEffort != nil && straightforward == nil {
		return nil, errors.New("--subagent-straightforward-effort requires --subagent-straightforward")
	}
	if subtleEffort != nil && subtle == nil {
		return nil, errors.New("--subagent-subtle-effort requires --subagent-subtle")
	}
	if is(straightforwardEffort, "adaptive") || is(subtleEffort, "adaptive") {
		return nil, errors.New("adaptive sub-agent effort is not supported")
	}
	if orchestrators && (is(mode, "native") || is(mode, "none")) {
		return nil, errors.New("--orchestrators requires Mjolnir delegation (--subagents mjolnir)")
	}
	if straightforward != nil || subtle != nil || orchestrators && mode == nil {
		if mode != nil && *mode != "mjolnir" {
			return nil, errors.New("per-type sub-agent overrides require --subagents mjolnir or an omitted --subagents")
		}
		policy := &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir, Orchestrators: orchestrators}
		if straightforward != nil {
			policy.Straightforward = &core.SubagentChoice{Model: *straightforward, Effort: straightforwardEffort}
		}
		if subtle != nil {
			policy.Subtle = &core.SubagentChoice{Model: *subtle, Effort: subtleEffort}
		}
		return policy, nil
	}
	if mode == nil {
		return nil, nil
	}
	switch *mode {
	case "native":
		return &core.SubagentPolicy{Mode: core.SubagentPolicyNative}, nil
	case "mjolnir":
		return &core.SubagentPolicy{Mode: core.SubagentPolicyMjolnir, Orchestrators: orchestrators}, nil
	// Not offered in Create or Move: benchmarks use it for solo lanes.
	case "none":
		return &core.SubagentPolicy{Mode: core.SubagentPolicyNone}, nil
	}
	return nil, fmt.Errorf("unknown subagents mode %s; use native, mjolnir or none. %s", core.RustDebugString(*mode), remedy)
}

// turnReviewRemoved is said when a session asks for the review mode that was
// removed.
const turnReviewRemoved = "Per-turn review was removed. Use `--review push`, or omit `--review` to review before push when reviewers are configured."

// newSessionReview is the session's review overrides, or nil to follow all
// of `[review]`.
func newSessionReview(args *newArgs) (core.SessionReview, error) {
	return sessionReview(args.review, args.reviewers)
}

// sessionReview reads the review flags shared by `mj new` and `mj import`;
// nil follows all of `[review]`.
func sessionReview(trigger *string, choices []string) (core.SessionReview, error) {
	if len(choices) > core.MaxReviewers {
		return nil, errors.New("at most three --reviewer values are allowed")
	}
	reviewers := make([]core.SubagentChoice, 0, len(choices))
	for _, value := range choices {
		model, effort := value, (*string)(nil)
		if index := strings.LastIndex(value, ":"); index >= 0 {
			prefix, suffix := value[:index], value[index+1:]
			switch {
			case slices.Contains([]string{"minimal", "low", "medium", "high", "xhigh", "max"}, suffix):
				model, effort = prefix, &suffix
			case suffix == "":
				model = prefix
			}
		}
		if strings.TrimSpace(model) == "" {
			return nil, errors.New("reviewer model must not be empty")
		}
		reviewers = append(reviewers, core.SubagentChoice{Model: model, Effort: effort})
	}
	if trigger == nil {
		if len(reviewers) == 0 {
			return nil, nil
		}
		return core.SessionReviewInherit{Reviewers: &reviewers}, nil
	}
	var listed *[]core.SubagentChoice
	if len(reviewers) > 0 {
		listed = &reviewers
	}
	switch *trigger {
	case "off":
		return core.SessionReviewOff{}, nil
	case "push":
		return core.SessionReviewPush{Reviewers: listed}, nil
	case "turn":
		return nil, errors.New(turnReviewRemoved)
	}
	return nil, fmt.Errorf("unknown --review value %s; use off or push", core.RustDebugString(*trigger))
}

// quickBundleForDirectory makes the quick bundle for a checkout on this
// computer (go.rs's quick_bundle_for_directory, which `mj new` reaches only
// through this).
func quickBundleForDirectory(ctx context.Context, directory string) (string, error) {
	if !utf8.ValidString(directory) {
		return "", errors.New("project directory must be valid UTF-8")
	}
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return "", err
	}
	defer daemon.Close()
	created, err := daemon.CreateProject(ctx, []string{directory})
	if err != nil {
		return "", err
	}
	return created.BundleID, nil
}

// newSession creates a session and delivers its optional first prompt.
func newSession(ctx context.Context, args newArgs, requestedWorkspace *string) error {
	subagents, err := newSubagentPolicy(&args)
	if err != nil {
		return err
	}
	prompt, err := readPrompt(args.prompt, args.promptFile)
	if err != nil {
		return err
	}
	api, workspaceID, targetID, projectDirectory, options, err := inferNewSessionInputs(ctx, &args, requestedWorkspace)
	if err != nil {
		return err
	}
	bundleID, projectDirectory, err := resolveNewSessionProjectTarget(ctx, api, args.bundle, projectDirectory, targetID, options, quickBundleForDirectory)
	if err != nil {
		return err
	}
	var oracle *bool
	if args.oracle != nil {
		enabled := *args.oracle == "enabled"
		oracle = &enabled
	}
	oracleChoice, err := oracleOverride(args.subagentOracle, args.subagentOracleEffort, args.oracle)
	if err != nil {
		return err
	}
	review, err := newSessionReview(&args)
	if err != nil {
		return err
	}
	var memoryBytes *uint64
	if args.memoryGiB != nil {
		if *args.memoryGiB > (1<<64-1)>>30 {
			return errors.New("--memory-gib is too large")
		}
		bytes := *args.memoryGiB << 30
		memoryBytes = &bytes
	}
	request := server.StartSessionRequest{
		RememberContainerSize: false,
		Subagents:             subagents,
		Oracle:                oracle,
		OracleChoice:          oracleChoice,
		Review:                review,
		NoProjectMemory:       args.noProjectMemory,
		NoGithubEvents:        args.noGitHubEvents,
		NoGithubCredentials:   args.noGitHubCredentials,
		NotificationsIgnored:  args.ignoreNotifications,
		CreateManagedWorktree: nil,
		At:                    args.at,
		Branch:                args.branch,
		Base:                  args.base,
		NoLaterHistory:        args.noLaterHistory,
		WorkspaceID:           &workspaceID,
		ProfileID:             args.profile,
		TargetID:              targetID,
		BundleID:              bundleID,
		ProjectDirectory:      projectDirectory,
		Title:                 args.title,
		Model:                 args.model,
		Effort:                args.effort,
		Prompt:                prompt,
		Cpus:                  args.cpus,
		MemoryBytes:           memoryBytes,
	}
	response, err := api.start(ctx, &request)
	if err != nil {
		return nameLaunchFlags(err)
	}
	if response.Notice != nil {
		if err := writeStartNotice(os.Stderr, *response.Notice); err != nil {
			return err
		}
	}
	if args.json {
		return printJSON(response)
	}
	fmt.Printf("session %s\n", response.SessionID)
	if response.TurnID != nil {
		fmt.Printf("turn %d\n", *response.TurnID)
	}
	return nil
}

// resolveNewSessionProjectTarget turns a project directory on a target that
// runs isolated projects into a quick bundle; it answers the bundle and the
// project directory the session starts with.
func resolveNewSessionProjectTarget(
	ctx context.Context,
	api *apiClient,
	bundleID *string,
	projectDirectory *string,
	targetID *string,
	options *server.LaunchOptions,
	createQuickBundle func(ctx context.Context, directory string) (string, error),
) (bundle *string, directory *string, err error) {
	if bundleID == nil && projectDirectory != nil && targetID != nil {
		if options == nil {
			launch, err := api.launchOptions(ctx)
			if err != nil {
				return nil, nil, err
			}
			options = &launch
		}
		for _, target := range options.Targets {
			if target.ID == *targetID && !target.RequiresProjectDirectory {
				created, err := createQuickBundle(ctx, *projectDirectory)
				if err != nil {
					return nil, nil, err
				}
				return &created, nil, nil
			}
		}
	}
	return bundleID, projectDirectory, nil
}

// inferNewSessionInputs settles what `mj new` was not told: the workspace,
// the target, and the project directory, reading the daemon's launch options
// and the current folder. It refuses, naming every missing flag at once,
// when it cannot settle them.
func inferNewSessionInputs(ctx context.Context, args *newArgs, requestedWorkspace *string) (
	api *apiClient,
	workspaceID string,
	targetID *string,
	projectDirectory *string,
	options *server.LaunchOptions,
	err error,
) {
	var open []string
	chosenWorkspace := args.workspaceID
	if chosenWorkspace == nil && requestedWorkspace != nil {
		if err := refuseUnknownWorkspace(ctx, *requestedWorkspace); err != nil {
			return nil, "", nil, nil, nil, err
		}
		id, err := resolveStoreWorkspace(ctx, requestedWorkspace)
		if err != nil {
			return nil, "", nil, nil, nil, err
		}
		chosenWorkspace = &id
	}
	absoluteOrBundle := args.bundle != nil
	if args.projectDirectory != nil {
		absoluteOrBundle = filepath.IsAbs(*args.projectDirectory)
	}
	if chosenWorkspace != nil && args.target != nil && (args.profile != nil || args.model != nil) && absoluteOrBundle {
		connected, err := apiClientConnect(ctx)
		if err != nil {
			return nil, "", nil, nil, nil, err
		}
		return connected, *chosenWorkspace, args.target, args.projectDirectory, nil, nil
	}
	var listed []core.WorkspaceRecord
	listedOK := false
	if chosenWorkspace == nil {
		listed, listedOK = listedWorkspacesWithoutStarting(ctx)
	}
	var connected *apiClient
	if listedOK && len(listed) == 0 {
		// A daemon that is not running is not started just to list nothing.
		if running, err := apiClientConnectRunning(ctx); err == nil {
			connected = running
		}
	} else if connected, err = apiClientConnect(ctx); err != nil {
		return nil, "", nil, nil, nil, err
	}
	if chosenWorkspace == nil && !listedOK {
		listed, listedOK = listedWorkspacesWithoutStarting(ctx)
	}

	var launch *server.LaunchOptions
	if connected != nil {
		read, err := connected.launchOptions(ctx)
		if err != nil {
			return nil, "", nil, nil, nil, err
		}
		launch = &read
	}
	var selectedTarget *server.LaunchTarget
	if launch != nil {
		autoPrimary := args.profile == nil && args.model == nil && args.effort == nil && launch.PrimaryConfigured
		profiles := make([]string, 0, len(launch.Profiles))
		for _, profile := range launch.Profiles {
			profiles = append(profiles, profile.ID)
		}
		var targets []string
		for _, target := range launch.Targets {
			if !target.RuntimeMissing {
				targets = append(targets, target.ID)
			}
		}
		resolve := func(needProfile bool) (string, []string) {
			_, target, choices := server.ResolveLaunchChoices(args.profile, args.target, needProfile, profiles, targets, launch.Default)
			return target, choices
		}
		if _, choices := resolve(args.model == nil && !autoPrimary); choices != nil {
			for _, choice := range choices {
				choice = strings.Replace(choice, "a profile_id", "--profile ID", 1)
				open = append(open, strings.Replace(choice, "a target_id", "--target ID", 1))
			}
		}
		if target, choices := resolve(false); choices == nil {
			for index := range launch.Targets {
				if launch.Targets[index].ID == target {
					selectedTarget = &launch.Targets[index]
					break
				}
			}
		}
	}
	if launch == nil {
		if args.profile == nil && args.model == nil {
			open = append(open, "--profile ID")
		}
		if args.target == nil {
			open = append(open, "--target ID")
		}
	}
	colocated := connected != nil && connected.daemonOnClientFilesystem()
	local := colocated && selectedTarget != nil && selectedTarget.Kind != "ssh-bare"
	var folderPath string
	var folderErr error
	folderResolved := false
	folder := func() (string, error) {
		if !folderResolved {
			folderResolved = true
			folderPath, folderErr = func() (string, error) {
				var path string
				if args.projectDirectory != nil {
					expanded, err := core.ExpandLocal(*args.projectDirectory)
					if err != nil {
						return "", err
					}
					path = expanded
				} else {
					current, err := os.Getwd()
					if err != nil {
						return "", fmt.Errorf("read the current folder: %w", err)
					}
					path = current
				}
				canonical, err := filepath.EvalSymlinks(path)
				if err == nil {
					canonical, err = filepath.Abs(canonical)
				}
				if err != nil {
					return "", fmt.Errorf("open folder %s: %w", path, err)
				}
				return canonical, nil
			}()
		}
		return folderPath, folderErr
	}

	if chosenWorkspace == nil {
		knownEmpty := listedOK && len(listed) == 0
		if local {
			if _, err := folder(); err == nil {
				// go.rs's inferred_workspace reads the folder's saved
				// workspace binding; the Go build does not have it yet.
				return nil, "", nil, nil, nil, fmt.Errorf("choosing the workspace from the current folder (go.rs's project bindings): %w; pass --workspace NAME", errNotPorted)
			}
		}
		switch {
		case len(listed) == 1:
			id := listed[0].ID
			chosenWorkspace = &id
		case knownEmpty:
			open = append(open, "--workspace NAME (this instance has none yet. Create one with `mj workspaces create NAME`)")
		case len(listed) == 0:
			open = append(open, "--workspace NAME (see `mj workspaces`)")
		default:
			names := make([]string, 0, len(listed))
			for _, workspace := range listed {
				names = append(names, workspace.Name)
			}
			open = append(open, fmt.Sprintf("--workspace NAME (one of: %s)", strings.Join(names, ", ")))
		}
	}
	chosenDirectory := args.projectDirectory
	if chosenDirectory == nil && args.bundle == nil {
		switch {
		case selectedTarget != nil && selectedTarget.RequiresProjectDirectory && local:
			if path, err := folder(); err != nil {
				open = append(open, fmt.Sprintf("--project-directory PATH (%s)", err))
			} else {
				chosenDirectory = &path
			}
		case selectedTarget != nil && selectedTarget.RequiresProjectDirectory:
			host := "the target host"
			if selectedTarget.Host != nil {
				host = *selectedTarget.Host
			}
			open = append(open, fmt.Sprintf("--project-directory PATH (the folder on %s)", host))
		default:
			open = append(open, "--bundle ID or --project-directory PATH")
		}
	}
	if len(open) > 0 {
		return nil, "", nil, nil, nil, fmt.Errorf("mj new needs more than it was given. Pass:\n  %s", strings.Join(open, "\n  "))
	}
	if local && args.projectDirectory != nil {
		path, err := folder()
		if err != nil {
			return nil, "", nil, nil, nil, err
		}
		chosenDirectory = &path
	}
	// Pin the target used to interpret the directory even if the saved
	// default changes between reading launch options and creating a session.
	pinnedTarget := args.target
	if selectedTarget != nil {
		id := selectedTarget.ID
		pinnedTarget = &id
	}
	if connected == nil {
		return nil, "", nil, nil, nil, errors.New("launch options are unavailable")
	}
	if chosenWorkspace == nil {
		return nil, "", nil, nil, nil, errors.New("no workspace was selected")
	}
	return connected, *chosenWorkspace, pinnedTarget, chosenDirectory, launch, nil
}

// writeStartNotice writes a start notice the daemon sent, with terminal
// controls removed.
func writeStartNotice(output io.Writer, notice string) error {
	_, err := fmt.Fprintln(output, core.SanitizeTerminalText(notice))
	return err
}

// nameLaunchFlags names the command-line flags where a refused start names
// the API's fields.
//
// The API speaks to every client, so its refusals name request fields such
// as `profile_id`, `target_id` and `at`; someone at a shell typed
// `--profile`, `--target` and `--at`, and that is what they need to read
// (F-9). Refusals quote the one-word fields in backticks, so only field
// names are replaced.
func nameLaunchFlags(err error) error {
	message := err.Error()
	named := message
	for _, replacement := range [][2]string{
		{"profile_id", "--profile"},
		{"target_id", "--target"},
		{"bundle_id", "--bundle"},
		{"project_directory", "--project-directory"},
		{"`no_later_history`", "--no-later-history"},
		{"`at`", "--at"},
		{"`branch`", "--branch"},
		{"`base`", "--base"},
		{"`cpus`", "--cpus"},
		{"`memory_bytes`", "--memory-gib"},
	} {
		named = strings.ReplaceAll(named, replacement[0], replacement[1])
	}
	if named == message {
		return err
	}
	return errors.New(named)
}

// nameSuspendFlags names the `mj suspend` flag where a refusal names the API
// field, as nameLaunchFlags does for `mj new` (launch finding R2-3).
func nameSuspendFlags(err error) error {
	message := err.Error()
	named := strings.ReplaceAll(message, "acknowledge_unpublished_work=true", "--acknowledge-unpublished-work")
	if named == message {
		return err
	}
	return errors.New(named)
}

// clearQueueArgs are the arguments of `mj clear-queue`.
type clearQueueArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// commandID is a stable producer-chosen identity; repeat it after an
	// ambiguous reply.
	commandID *string
	// json prints the API response as JSON.
	json bool
}

// bind declares the clear-queue options on cmd.
func (args *clearQueueArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("command_id", "Stable producer-chosen identity; repeat it after an ambiguous reply", storeOptional(&args.commandID))
	meta.flag(&args.json, "json", "Print the API response as JSON")
	leaf(cmd, run)
}

// clearQueue cancels queued prompts while preserving the session and its
// current turn.
func clearQueue(ctx context.Context, args clearQueueArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	result, err := api.clearQueue(ctx, args.session, args.commandID)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(result)
	}
	fmt.Printf("queue cancellation %d accepted\n", result.TurnID)
	return nil
}

// prompt sends a prompt as the user and waits for the turn it became.
func prompt(ctx context.Context, args promptArgs) error {
	calling, err := callingSession()
	if err != nil {
		return err
	}
	return promptFrom(ctx, args, calling)
}

// promptFrom is prompt with the calling session given. A prompt sent this
// way reads exactly as the user's own, so it refuses to run inside a
// session.
func promptFrom(ctx context.Context, args promptArgs, callingSession *string) error {
	// A prompt sent this way reads exactly as the user's own. An agent sends
	// as itself instead, and its message is labelled with its session.
	if callingSession != nil {
		return errors.New("`mj prompt` sends as the user, so it cannot run inside a session (MJ_SESSION_ID is set). Send with `mj message`, then follow with `mj wait`.")
	}
	text, err := readPrompt(args.text, args.promptFile)
	if err != nil {
		return err
	}
	if text == nil {
		return errors.New("pass the prompt text, --prompt-file, or `-` to read standard input")
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	var accepted server.PromptResponse
	if args.commandID != nil {
		accepted, err = api.promptWithID(ctx, args.session, *text, args.commandID)
	} else {
		accepted, err = api.prompt(ctx, args.session, *text)
	}
	if err != nil {
		return err
	}
	// Named before the wait, so a caller whose wait is cut off can go on
	// with `mj wait --turn`.
	if !args.json {
		fmt.Fprintf(os.Stderr, "turn %d\n", accepted.TurnID)
	}
	// Only the wait follows the daemon across an upgrade; the prompt was
	// accepted once and is never sent again.
	turn := accepted.TurnID
	response, err := waitFollowingHandoffs(ctx, api, args.session,
		sessionruntime.WaitRequest{ReturnOnInput: args.returnOnInput, TurnID: &turn, TimeoutSecs: args.timeout},
		func(ctx context.Context, request *sessionruntime.WaitRequest) (*apiClient, error) {
			return followHandoff(ctx, waitContinuation(args.session, request, args.json))
		})
	if err != nil {
		return err
	}
	return reportWait(&response, args.json)
}

// wait blocks until a turn ends and prints how it ended.
func wait(ctx context.Context, args waitArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	response, err := waitFollowingHandoffs(ctx, api, args.session,
		sessionruntime.WaitRequest{ReturnOnInput: args.returnOnInput, TurnID: args.turn, TimeoutSecs: args.timeout},
		func(ctx context.Context, request *sessionruntime.WaitRequest) (*apiClient, error) {
			return followHandoff(ctx, waitContinuation(args.session, request, args.json))
		})
	if err != nil {
		return err
	}
	return reportWait(&response, args.json)
}

// followHandoff connects to the daemon that replaced one being upgraded.
// When that daemon is a newer protocol or release, which this client cannot
// speak to, it finishes the command under the daemon's own build, as
// dashboards do. continuation names only what is left of the command, so
// nothing is repeated.
func followHandoff(ctx context.Context, continuation []string) (*apiClient, error) {
	connected, connectErr := apiClientConnectAfterHandoff(ctx)
	if connectErr == nil {
		return connected, nil
	}
	target, err := upgradedDaemonExecutable(ctx)
	if err != nil {
		return nil, err
	}
	if target == nil {
		return nil, connectErr
	}
	return nil, continueUnderUpgradedBuild(target, continuation)
}

// waitContinuation is the rest of a wait: the same session and turn, the
// time left, and the same output form.
func waitContinuation(session string, request *sessionruntime.WaitRequest, json bool) []string {
	args := []string{"wait", "--session", session}
	if request.TurnID != nil {
		args = append(args, "--turn", strconv.FormatUint(*request.TurnID, 10))
	}
	if request.TimeoutSecs != nil {
		args = append(args, "--timeout", strconv.FormatUint(*request.TimeoutSecs, 10))
	}
	if request.ReturnOnInput {
		args = append(args, "--return-on-input")
	}
	if json {
		args = append(args, "--json")
	}
	return args
}

// eventsContinuation is the rest of an event stream: the same filter, after
// the last event seen.
func eventsContinuation(filter *core.ApiEventFilter, afterSeq *uint64) []string {
	args := []string{"events"}
	if filter.SessionID != nil {
		args = append(args, "--session", *filter.SessionID)
	}
	if filter.WorkspaceID != nil {
		args = append(args, "--workspace-id", *filter.WorkspaceID)
	}
	if afterSeq != nil {
		args = append(args, "--after-seq", fmt.Sprint(*afterSeq))
	}
	return args
}

// waitFollowingHandoffs waits on a session, following the daemon across
// automatic upgrades.
//
// A handoff ends the wait the old daemon was serving. A wait is a read, so
// the next daemon is asked again for the same turn, with what is left of the
// time budget: the handoff neither extends the wait nor resends a prompt.
// Each retry follows a daemon that closed admission and is exiting, and
// reconnect waits for the daemon that replaces it.
func waitFollowingHandoffs(
	ctx context.Context,
	first *apiClient,
	session string,
	request sessionruntime.WaitRequest,
	reconnect func(ctx context.Context, request *sessionruntime.WaitRequest) (*apiClient, error),
) (server.WaitResponse, error) {
	started := time.Now()
	budget := server.DefaultWaitSecs
	if request.TimeoutSecs != nil {
		budget = *request.TimeoutSecs
	}
	api := first
	for {
		response, err := api.wait(ctx, session, &request)
		if err == nil || !isDaemonHandoff(err) {
			return response, err
		}
		fmt.Fprintln(os.Stderr, "The Mjolnir daemon is being replaced by an upgrade; waiting on the new daemon.")
		elapsed := uint64(time.Since(started) / time.Second)
		remaining := uint64(1)
		if budget > elapsed {
			remaining = max(budget-elapsed, 1)
		}
		request.TimeoutSecs = &remaining
		if api, err = reconnect(ctx, &request); err != nil {
			return server.WaitResponse{}, err
		}
	}
}

// reportWait prints a wait result, and fails the process when the turn did
// not finish so a shell script can branch on it.
func reportWait(response *server.WaitResponse, json bool) error {
	if json {
		if err := printJSON(response); err != nil {
			return err
		}
	} else {
		printLines(waitReportLines(response))
		if len(response.PendingElicitations) > 0 {
			if err := printJSON(response.PendingElicitations); err != nil {
				return err
			}
		}
	}
	switch {
	case response.Outcome == sessionruntime.WaitOutcomeFinished || response.Outcome == sessionruntime.WaitOutcomeInputRequired:
		return nil
	case suspendedCleanly(response):
		return nil
	}
	return fmt.Errorf("the turn ended as %s", outcomeName(response.Outcome))
}

// suspendedCleanly is whether a wait ended because the session was
// suspended as asked, rather than because a resume or a close failed.
// `mj suspend` tells the user to watch with `mj wait`, so this ending is the
// success it was waiting for.
func suspendedCleanly(response *server.WaitResponse) bool {
	return response.Outcome == sessionruntime.WaitOutcomeStopped &&
		response.Session.Lifecycle == sessionruntime.ViewerLifecycleCategorySuspended &&
		response.Session.Error == nil
}

// waitReportLines is what `mj wait` prints, one line per entry.
//
// Separate from the printing so the content can be tested: a turn that
// failed used to print the bare word "error" and leave the reason in the
// transcript, where automation never saw it (#1020).
func waitReportLines(response *server.WaitResponse) []string {
	if suspendedCleanly(response) {
		return []string{"session suspended"}
	}
	var lines []string
	if response.Outcome == sessionruntime.WaitOutcomeError && response.Session.Error != nil {
		lines = append(lines, "session error: "+*response.Session.Error)
	}
	// No number: `turn_id` is where the turn's prompt sits in the transcript,
	// not a count of anything a reader can use, so it stays in --json (I2-5).
	summary := "turn " + outcomeName(response.Outcome)
	// How the turn ended, in the words `mj sessions` and the sub-agent notice
	// use; the harness's own spelling (`EndTurn`) stays in --json (R12-1).
	if response.StopReason != nil {
		summary += fmt.Sprintf(" (%s)", core.TurnOutcomeKindCompleted{StopReason: *response.StopReason})
	}
	if response.ElapsedMs != nil {
		summary += fmt.Sprintf(" in %.1fs", float64(max(*response.ElapsedMs, 0))/1000.0)
	}
	if response.ToolCalls != nil && *response.ToolCalls > 0 {
		plural := "s"
		if *response.ToolCalls == 1 {
			plural = ""
		}
		summary += fmt.Sprintf(" · %d tool call%s", *response.ToolCalls, plural)
	}
	// A wait on a turn answers when that turn ends, whatever the session does
	// after it; say what that is, in the dashboard's words.
	if response.Next != nil {
		activity := client.SessionActivityOfState(*response.Next)
		if retry := response.CapacityRetry; retry != nil {
			activity.CapacityRetry = &core.CapacityRetry{Attempt: retry.Attempt, RetryAtMs: retry.RetryAtMs}
		} else {
			activity.CapacityRetry = nil
		}
		activity.QuotaRecovery = response.QuotaRecovery
		if background := response.Session.BackgroundWork; background != nil {
			activity.BackgroundCommands = slices.Clone(background.Tasks)
		}
		now := uint64(max(core.EpochMillis()/1000, 0))
		summary += "; the session is not idle: " + client.FormatActivityClock(now, nil, &activity)
	}
	lines = append(lines, summary)
	// A wait for turn N answers with the latest turn that ended, which can be
	// another turn: a later one, an earlier one that ended after N was
	// removed from the queue, or one the session ran without a prompt. Only
	// an answer about a finished turn has a turn number.
	if requested := response.RequestedTurnID; requested != nil && response.TurnNumber != nil &&
		(response.TurnID == nil || *response.TurnID != *requested) {
		answered := "a turn the session ran without a prompt"
		if response.TurnID != nil {
			answered = fmt.Sprintf("turn %d", *response.TurnID)
		}
		lines = append(lines, fmt.Sprintf("this is the outcome of %s, not of turn %d; a session keeps only its latest turn's outcome and reply", answered, *requested))
	}
	// The wait's message, the diagnostic and the agent's last message are
	// often one sentence (a Codex quota error was printed three times,
	// J-25), so each is printed only when it says something new.
	said := func(text string) bool {
		for _, line := range lines {
			if strings.TrimSpace(line) == strings.TrimSpace(text) {
				return true
			}
		}
		return false
	}
	if response.Message != nil {
		lines = append(lines, *response.Message)
	}
	if response.Outcome == sessionruntime.WaitOutcomeTimeout && len(response.PendingElicitations) > 0 {
		sessionID := response.Session.ID
		lines = append(lines, fmt.Sprintf("input is pending; inspect it with `mj elicitations --session %s`, answer with `mj respond --session %s --elicitation <id> '<json>'`, or use `mj prompt --return-on-input`", sessionID, sessionID))
	}
	// Why the turn ended, when the worker recorded a reason.
	if diagnostic := response.Diagnostic; diagnostic != nil && response.Outcome != sessionruntime.WaitOutcomeFinished && !said(diagnostic.Message) {
		lines = append(lines, diagnostic.Message)
	}
	if recovery := response.QuotaRecovery; recovery != nil {
		lines = append(lines, recovery.Notice)
	}
	if retry := response.CapacityRetry; retry != nil {
		lines = append(lines, fmt.Sprintf("a server retry is armed (attempt %d); do not send another prompt yet", retry.Attempt))
	}
	// A turn that is still running and a worker the daemon cannot see look
	// identical from a timeout alone, so name the relay when it is at fault.
	if relay := response.Relay; response.Outcome == sessionruntime.WaitOutcomeTimeout && relay != nil && relay.State != server.RelayStateConnected {
		line := "the daemon's view of this session is " + relayStateName(relay.State)
		if relay.Detail != nil {
			line += ": " + *relay.Detail
		}
		lines = append(lines, line)
	}
	if final := response.FinalMessage; final != nil && !said(*final) {
		lines = append(lines, "", *final)
	}
	return lines
}

// sessionReportLines is what `mj sessions --session` prints for one session,
// one line per entry.
func sessionReportLines(session *server.ApiSession, nowMS int64) []string {
	lines := []string{fmt.Sprintf("%s  %s  %s", session.ID, listedState(session), session.Title)}
	if operation := session.Operation; operation != nil {
		kind := map[server.ViewerOperationKind]string{
			server.ViewerOperationKindCreate:     "create",
			server.ViewerOperationKindResume:     "resume",
			server.ViewerOperationKindMove:       "move",
			server.ViewerOperationKindSuspend:    "suspend",
			server.ViewerOperationKindDestroy:    "destroy",
			server.ViewerOperationKindStop:       "stop",
			server.ViewerOperationKindCleanup:    "cleanup",
			server.ViewerOperationKindCheckpoint: "checkpoint",
		}[operation.Kind]
		lines = append(lines, fmt.Sprintf("%s started %s", kind, operationStartTime(operation.StartedAtEpochSeconds)))
		for _, stage := range operation.Stages {
			lines = append(lines, fmt.Sprintf("  %s since %s", stage.Label, operationStartTime(stage.StartedAtEpochSeconds)))
		}
	}
	if relay := session.Relay; relay != nil {
		lines = append(lines, "worker relay is "+relayStateName(relay.State))
	}
	// Silence is reported, never acted on. A turn waiting on a long build
	// is quiet and healthy, so this says what is true and leaves the
	// decision — keep waiting, or `mj interrupt-turn` — to the reader.
	if session.ActivityState != nil {
		if note, ok := core.SilenceNote(session.ActivityState, nowMS); ok {
			lines = append(lines, "running, "+note)
		}
	}
	if session.Error != nil {
		lines = append(lines, "error: "+*session.Error)
	}
	if session.StorageProblem != nil {
		lines = append(lines, *session.StorageProblem)
	}
	if session.ProfileLoginError != nil {
		lines = append(lines, fmt.Sprintf("profile %s: %s", session.ProfileID, *session.ProfileLoginError))
	}
	if outcome := session.LastTurnOutcome; outcome != nil {
		lines = append(lines, fmt.Sprintf("last turn %s", outcome.Outcome))
	}
	return lines
}

// operationStartTime is an operation's start as RFC 3339 in UTC, to the
// second.
func operationStartTime(seconds uint64) string {
	// chrono's DateTime::from_timestamp covers years -262143 to 262142.
	const maximum = 8210266876799
	if seconds > maximum {
		return fmt.Sprintf("%d seconds since the Unix epoch", seconds)
	}
	return time.Unix(int64(seconds), 0).UTC().Format("2006-01-02T15:04:05Z")
}

// relayStateName is the relay state in the words `mj wait` and
// `mj sessions` use.
func relayStateName(state server.RelayState) string {
	switch state {
	case server.RelayStateConnected:
		return "connected"
	case server.RelayStateDisconnected:
		return "not attached"
	case server.RelayStateUnreachable:
		return "unreachable"
	case server.RelayStateTargetMissing:
		return "missing its target"
	case server.RelayStateProjectionIntegrity:
		return "out of step with the worker's events"
	}
	panic(fmt.Sprintf("unknown relay state %q", string(state)))
}

// outcomeName is the wait outcome's wire name.
func outcomeName(outcome sessionruntime.WaitOutcome) string {
	switch outcome {
	case sessionruntime.WaitOutcomeFinished:
		return "finished"
	case sessionruntime.WaitOutcomeInputRequired:
		return "input_required"
	case sessionruntime.WaitOutcomeError:
		return "error"
	case sessionruntime.WaitOutcomeCancelled:
		return "cancelled"
	case sessionruntime.WaitOutcomeQuotaLimit:
		return "quota_limit"
	case sessionruntime.WaitOutcomeTimeout:
		return "timeout"
	case sessionruntime.WaitOutcomeStopped:
		return "stopped"
	}
	panic(fmt.Sprintf("unknown wait outcome %q", string(outcome)))
}

// transcript pages through a session's transcript.
func transcript(ctx context.Context, args transcriptArgs) error {
	roles, err := requestedTranscriptRoles(&args)
	if err != nil {
		return err
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	return printTranscript(ctx, api, args, roles)
}

// printTranscript reads the transcript page args and roles ask for from api
// and prints it.
func printTranscript(ctx context.Context, api *apiClient, args transcriptArgs, roles []core.TranscriptRole) error {
	page, err := api.transcript(ctx, args.session, args.afterSeq, args.limit, roles, args.finishedOnly)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(page)
	}
	for _, item := range page.Items {
		fmt.Printf("[%d] %s\n", item.Seq, item.Role)
		if strings.TrimSpace(item.Text) != "" {
			fmt.Println(core.SanitizeTerminalText(strings.TrimRight(item.Text, rustWhitespace)))
		}
		fmt.Println()
	}
	fmt.Printf("next after seq %d; latest seq %d\n", page.NextAfterSeq, page.LatestSeq)
	return nil
}

// rustWhitespace is what Rust's `str::trim_end` trims: Unicode White_Space.
const rustWhitespace = "\t\n\v\f\r \u0085                 　"

// diff prints a unified diff of a session's work, and warns when HEAD does
// not descend from the base.
func diff(ctx context.Context, args diffArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	// The metadata form is always requested: the plain form prints only the
	// patch, but the divergence warning comes from the same answer.
	body, err := api.diff(ctx, args.session, args.base, true)
	if err != nil {
		return err
	}
	var details checkpoint.SessionDiff
	if err := json.Unmarshal([]byte(body), &details); err != nil {
		return fmt.Errorf("decode session diff metadata: %w", err)
	}
	if args.json {
		return printJSON(details)
	}
	// A redirected patch keeps its exact bytes (CRLF, control characters) so
	// it still applies; only a terminal gets the inert rendering.
	patch := details.Diff
	if term.IsTerminal(os.Stdout.Fd()) {
		patch = core.SanitizeTerminalText(patch)
	}
	if _, err := io.WriteString(os.Stdout, patch); err != nil {
		return fmt.Errorf("write the session diff: %w", err)
	}
	if details.HeadDescendsFromBase != nil && !*details.HeadDescendsFromBase {
		head, base := []rune(details.Head), []rune(details.Base)
		fmt.Fprintf(os.Stderr, "warning: HEAD %s does not descend from base %s; the diff includes history changes, not only session work. Pass --base to pick another base.\n",
			string(head[:min(12, len(head))]), string(base[:min(12, len(base))]))
	}
	return nil
}

// export gets a session's work out as a patch, a pushed branch, or a git
// bundle.
func export(ctx context.Context, args exportArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	kind := server.ExportKindPatch
	switch args.kind {
	case exportKindArgBranch:
		kind = server.ExportKindBranch
	case exportKindArgBundle:
		kind = server.ExportKindBundle
	}
	result, err := api.export(ctx, args.session, &server.ExportRequest{Kind: kind, Branch: args.branch})
	if err != nil {
		return err
	}
	switch result := result.(type) {
	case exportResultBranch:
		if args.json {
			return printJSON(result.branch)
		}
		fmt.Printf("pushed %s to %s\n", result.branch.Branch, result.branch.Remote)
		// A push carries commits and nothing else, and the agent may have
		// left work it never committed (F-13).
		fmt.Printf("only committed work was pushed; uncommitted changes stay in the session (`mj diff --session %s` shows all of its work)\n", args.session)
		return nil
	case exportResultBytes:
		format := "patch"
		if args.kind == exportKindArgBundle {
			format = "bundle"
		}
		return writeBytes(result.bytes, args.out, args.json, format)
	}
	panic(fmt.Sprintf("unknown export result %T", result))
}

// fetch copies a file, or a directory as a zip archive, from a session's
// target into this machine's temporary directory and prints where it landed.
func fetch(ctx context.Context, args fetchArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	fetched, err := api.fetch(ctx, args.session, args.path, args.ls)
	if err != nil {
		return err
	}
	defer fetched.Close()
	if args.ls {
		var listing checkpoint.SessionListing
		if err := json.NewDecoder(fetched.Reader()).Decode(&listing); err != nil {
			return fmt.Errorf("decode the listing: %w", err)
		}
		if args.json {
			return printJSON(listing)
		}
		printLines(listingLines(&listing))
		return nil
	}
	if args.stdout {
		if _, err := io.Copy(os.Stdout, fetched.Reader()); err != nil {
			return fmt.Errorf("write the fetched path: %w", err)
		}
		return nil
	}
	saved, err := fetched.Save(os.TempDir(), args.session)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(saved)
	}
	fmt.Println(saved.Path)
	return nil
}

// listingLines is one line per entry, as `ls -l` puts it: the size
// right-aligned, the modification time in UTC, then the name, with `/` after
// a directory and the target after a symlink. A directory's own size says
// nothing about what it holds and differs between filesystems, so it shows
// as `-`.
func listingLines(listing *checkpoint.SessionListing) []string {
	size := func(entry *checkpoint.ListedEntry) string {
		if entry.Kind == checkpoint.ListedKindDirectory {
			return "-"
		}
		return strconv.FormatUint(entry.Size, 10)
	}
	width := 0
	for index := range listing.Entries {
		width = max(width, len(size(&listing.Entries[index])))
	}
	lines := make([]string, 0, len(listing.Entries))
	for index := range listing.Entries {
		entry := &listing.Entries[index]
		name := entry.Name
		switch {
		case entry.Kind == checkpoint.ListedKindDirectory:
			name += "/"
		case entry.Kind == checkpoint.ListedKindSymlink && entry.Target != nil:
			name += " -> " + *entry.Target
		}
		modified := "-"
		if entry.Modified != nil {
			modified = *entry.Modified
		}
		entrySize := size(entry)
		lines = append(lines, fmt.Sprintf("%s%s  %s  %s", strings.Repeat(" ", width-len(entrySize)), entrySize, padRight(modified, 20), name))
	}
	return lines
}

// sessions lists the sessions the daemon holds, or reports one session.
func sessions(ctx context.Context, args sessionsArgs, requestedWorkspace *string) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if args.session != nil {
		// An id that names no Mjolnir session may still name a SessionWiki
		// row, which is what an agent has after a search.
		session, err := api.sessionIfKnown(ctx, *args.session)
		if err != nil {
			return err
		}
		if session == nil {
			return wikiSession(ctx, api, *args.session, args.json)
		}
		if args.json {
			return printJSON(session)
		}
		printLines(sessionReportLines(session, core.EpochMillis()))
		return nil
	}
	var workspace *string
	if requestedWorkspace != nil {
		id, err := resolveStoreWorkspace(ctx, requestedWorkspace)
		if err != nil {
			return err
		}
		workspace = &id
	}
	list, err := api.sessionsInWorkspace(ctx, workspace, args.all)
	if err != nil {
		return err
	}
	// The daemon's host table is the one place that says which machine a
	// target runs on. Unfiltered JSON does not use it.
	var rows []core.HostCapacityRow
	if !args.json || args.target != nil || args.host != nil {
		daemon, err := ConnectOrStart(ctx)
		if err != nil {
			return err
		}
		rows, err = daemon.HostCapacity(ctx)
		daemon.Close()
		if err != nil {
			return err
		}
	}
	if list.Sessions, err = selectSessions(list.Sessions, rows, args.target, args.host); err != nil {
		return err
	}
	if args.json {
		return printJSON(list)
	}
	idWidth, stateWidth := 2, 5
	for index := range list.Sessions {
		idWidth = max(idWidth, utf8.RuneCountInString(list.Sessions[index].ID))
		stateWidth = max(stateWidth, utf8.RuneCountInString(listedState(&list.Sessions[index])))
	}
	fmt.Printf("%s  %s  TITLE\n", padRight("ID", idWidth), padRight("STATE", stateWidth))
	for index := range list.Sessions {
		session := &list.Sessions[index]
		fmt.Printf("%s  %s  %s\n", padRight(session.ID, idWidth), padRight(listedState(session), stateWidth), core.SanitizeTerminalText(session.Title))
		host, ok := hostOfTarget(rows, session.TargetID)
		if !ok {
			host = "-"
		}
		fmt.Printf("  target %s  host %s  profile %s\n", core.SanitizeTerminalText(session.TargetID), core.SanitizeTerminalText(host), core.SanitizeTerminalText(session.ProfileID))
		if session.ProfileLoginError != nil {
			fmt.Printf("  profile %s: %s\n", core.SanitizeTerminalText(session.ProfileID), core.SanitizeTerminalText(*session.ProfileLoginError))
		}
		if session.Attention == core.SessionAttentionFailed {
			fmt.Printf("  error details: run `mj sessions --session %s`\n", session.ID)
		}
	}
	return nil
}

// hostOfTarget is the host `mj hosts` lists targetID under; a fleet's own
// name for its instances.
func hostOfTarget(rows []core.HostCapacityRow, targetID string) (string, bool) {
	for _, row := range rows {
		for _, id := range row.TargetIDs {
			if id == targetID {
				return row.Host, true
			}
		}
	}
	return "", false
}

// selectSessions keeps the sessions on target and host. A name `mj hosts`
// does not know is an error that lists the names it does, not an empty
// list.
func selectSessions(sessions []server.ApiSession, rows []core.HostCapacityRow, target, host *string) ([]server.ApiSession, error) {
	known := func(names []string) string {
		slices.Sort(names)
		return core.SanitizeTerminalText(strings.Join(slices.Compact(names), ", "))
	}
	if target != nil {
		var names []string
		found := false
		for _, row := range rows {
			for _, id := range row.TargetIDs {
				names = append(names, id)
				found = found || id == *target
			}
		}
		if !found {
			return nil, fmt.Errorf("no session matched: no target %s; known targets: %s", core.RustDebugString(*target), known(names))
		}
	}
	if host != nil {
		var names []string
		found := false
		for _, row := range rows {
			names = append(names, row.Host)
			found = found || row.Host == *host
		}
		if !found {
			return nil, fmt.Errorf("no session matched: no host %s; known hosts: %s", core.RustDebugString(*host), known(names))
		}
	}
	selected := []server.ApiSession{}
	for _, session := range sessions {
		if target != nil && session.TargetID != *target {
			continue
		}
		if host != nil {
			if on, ok := hostOfTarget(rows, session.TargetID); !ok || on != *host {
				continue
			}
		}
		selected = append(selected, session)
	}
	return selected, nil
}

// listedState is the STATE column: the session's state, and "disk full"
// when its target cannot take writes. `--session` prints the whole reason.
func listedState(session *server.ApiSession) string {
	state := session.State
	switch session.Attention {
	case core.SessionAttentionNeedsInput:
		state += " (needs input)"
	case core.SessionAttentionImportant:
		state += " (important)"
	case core.SessionAttentionFailed:
		state += " (failed)"
	case core.SessionAttentionUnreachable:
		state += " (unreachable)"
	}
	if session.StorageProblem != nil {
		state += " (disk full)"
	}
	if session.ProfileLoginError != nil {
		state += " (login required)"
	}
	return state
}

// wikiSession reports one SessionWiki row for an id that names no Mjolnir
// session.
//
// `mine` is a Mjolnir session the daemon still has a record of, `archived`
// one whose record the archive job destroyed, and `native` another tool's
// session. All three can be continued with `mj resume --wiki <id>`.
func wikiSession(ctx context.Context, api *apiClient, wikiID string, json bool) error {
	info, err := api.wikiSession(ctx, wikiID)
	if err != nil {
		return err
	}
	if info == nil {
		return fmt.Errorf("no session %s: it names neither a Mjolnir session nor an indexed one", wikiID)
	}
	if json {
		return printJSON(info)
	}
	printLines(wikiSessionLines(info))
	return nil
}

// wikiSessionLines is what `mj sessions --session` prints for a SessionWiki
// row.
func wikiSessionLines(info *client.WikiSessionInfo) []string {
	status := map[client.WikiSessionStatus]string{
		client.WikiSessionStatusMine:     "mine",
		client.WikiSessionStatusArchived: "archived",
		client.WikiSessionStatusNative:   "native",
	}[info.Status]
	lines := []string{
		fmt.Sprintf("%s  %s  %s", info.WikiID, status, info.Title),
		fmt.Sprintf("%s at %s", info.Tool, info.Path),
	}
	if info.MjolnirSessionID != nil {
		lines = append(lines, "Mjolnir session "+*info.MjolnirSessionID)
	}
	if info.NothingToRestore {
		lines = append(lines, "it cannot be continued: it never received a prompt")
	} else {
		lines = append(lines, fmt.Sprintf("continue it with `mj resume --wiki %s`", info.WikiID))
	}
	return lines
}

// suspend saves a recovery copy and releases the environment. Acceptance is
// not completion.
func suspend(ctx context.Context, args suspendArgs) error {
	session, err := suspendSessionID(&args)
	if err != nil {
		return err
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if args.dryRun {
		preflight, err := api.suspendPreflight(ctx, session, true, args.withoutCheckpoint)
		if err != nil {
			return err
		}
		if args.json {
			return printJSON(preflight)
		}
		fmt.Print(formatSuspendDryRun(&preflight))
		return nil
	}
	if args.withoutCheckpoint {
		preflight, err := api.suspendPreflight(ctx, session, false, true)
		if err != nil {
			return err
		}
		hasVerified := preflight.ExistingCheckpoint != nil && preflight.ExistingCheckpoint.Verified
		if !hasVerified && !args.yes {
			return fmt.Errorf("Session %s has no verified checkpoint, so suspending without a checkpoint cannot be resumed. The session is still running; pass `--yes` to confirm stopping %s with no resumable checkpoint.", session, session)
		}
	}
	consent := core.SuspendConsent{WithoutCheckpoint: args.withoutCheckpoint, ConfirmNoCheckpoint: args.yes}
	if args.discardUntracked {
		consent.DiscardUntracked = core.UntrackedConsent{Kind: core.ConsentAny}
	}
	if args.abortOperation {
		consent.AbortOperations = core.OperationConsent{Kind: core.ConsentAny}
	}
	accepted, err := api.suspend(ctx, session, &consent)
	if err != nil {
		return nameSuspendFlags(err)
	}
	if args.json {
		return printJSON(jsonvalue.Object{
			{Key: "session_id", Value: session},
			{Key: "accepted", Value: true},
			{Key: "operation", Value: "suspend"},
			{Key: "stopped_subagents", Value: accepted.StoppedSubagents},
			{Key: "subagents_not_handed_back", Value: accepted.SubagentsNotHandedBack},
			{Key: "warning", Value: accepted.Warning},
		})
	}
	if accepted.Warning != nil {
		fmt.Fprintf(os.Stderr, "warning: %s\n", *accepted.Warning)
	}
	fmt.Printf("suspension accepted for %s\n", session)
	fmt.Printf("`mj wait --session %s` returns once the session is suspended\n", session)
	return nil
}

// purgeUntrackedArgs remove legacy untracked checkpoint payloads from
// stopped sessions.
type purgeUntrackedArgs struct {
	// session is the session whose legacy untracked checkpoint payloads
	// should be removed. Required unless all; conflicts with all.
	session *string
	// all purges every session the daemon holds.
	all bool
	// dryRun reports on-disk bytes that would be reclaimed without rewriting
	// archives.
	dryRun bool
	// json prints the response as JSON.
	json bool
}

// bind declares the purge-untracked options on cmd.
func (args *purgeUntrackedArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", "Session whose legacy untracked checkpoint payloads should be removed", storeOptional(&args.session), requiredUnlessPresent("all"), conflictsWith("all"))
	meta.flag(&args.all, "all", "Purge every session the daemon holds")
	meta.flag(&args.dryRun, "dry_run", "Report on-disk bytes that would be reclaimed without rewriting archives")
	meta.flag(&args.json, "json", "Print the response as JSON")
	leaf(cmd, run)
}

// purgeUntracked removes legacy untracked checkpoint payloads from stopped
// sessions.
func purgeUntracked(ctx context.Context, args purgeUntrackedArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	response, err := api.purgeUntracked(ctx, &server.PurgeUntrackedRequest{SessionID: args.session, All: args.all, DryRun: args.dryRun})
	if err != nil {
		return err
	}
	report, status := purgeUntrackedConclusion(&response, args.json)
	if report != "" {
		if args.json {
			fmt.Println(report)
		} else {
			printLines(strings.Split(report, "\n"))
		}
	}
	return status
}

// purgeUntrackedConclusion is the text or JSON of a purge report, and
// whether the process should fail.
//
// A batch that rewrote some sessions and failed others still produces the
// whole report. The error comes after that report so a script sees every row
// and a non-zero status.
func purgeUntrackedConclusion(response *server.PurgeUntrackedResponse, json bool) (report string, status error) {
	if json {
		encoded, err := core.SerdeJSONPretty(response)
		if err != nil {
			return "", fmt.Errorf("serialize the API response: %w", err)
		}
		report = string(encoded)
	} else {
		report = strings.Join(purgeUntrackedLines(response), "\n")
	}
	return report, refuseFailedPurge(response)
}

// refuseFailedPurge fails when any session failed to purge.
func refuseFailedPurge(response *server.PurgeUntrackedResponse) error {
	failed := 0
	for _, session := range response.Sessions {
		if session.Status == server.PurgeUntrackedStatusFailed {
			failed++
		}
	}
	if failed > 0 {
		return fmt.Errorf("%d session(s) failed to purge untracked payloads", failed)
	}
	return nil
}

// purgeUntrackedLines is one line per session, then the total. A dry run
// says what it would reclaim.
func purgeUntrackedLines(response *server.PurgeUntrackedResponse) []string {
	var lines []string
	reason := func(reason *string, fallback string) string {
		if reason != nil {
			return *reason
		}
		return fallback
	}
	for _, session := range response.Sessions {
		switch session.Status {
		case server.PurgeUntrackedStatusPurged:
			lines = append(lines, fmt.Sprintf("%s: %d reclaimed", session.SessionID, session.BytesReclaimed))
		case server.PurgeUntrackedStatusDryRun:
			lines = append(lines, fmt.Sprintf("%s: would reclaim %d", session.SessionID, session.BytesReclaimed))
		case server.PurgeUntrackedStatusSkipped:
			lines = append(lines, fmt.Sprintf("%s: skipped (%s)", session.SessionID, reason(session.Reason, "skipped")))
		case server.PurgeUntrackedStatusFailed:
			lines = append(lines, fmt.Sprintf("%s: failed (%s)", session.SessionID, reason(session.Reason, "failed")))
		}
		for _, note := range session.StashUntrackedNotes {
			lines = append(lines, fmt.Sprintf("  stash %s %s (%s)", note.RepositoryID, note.Commit, note.Message))
		}
		for _, warning := range session.Warnings {
			lines = append(lines, "  warning: "+warning)
		}
	}
	if response.DryRun {
		lines = append(lines, fmt.Sprintf("Total: %d would be reclaimed", response.TotalBytesReclaimed))
	} else {
		lines = append(lines, fmt.Sprintf("Total: %d reclaimed", response.TotalBytesReclaimed))
	}
	return lines
}

// formatHumanBytes is a byte count in binary units with one decimal, or in
// bytes below a KiB.
func formatHumanBytes(bytes uint64) string {
	units := [...]string{"B", "KiB", "MiB", "GiB", "TiB"}
	size := float64(bytes)
	unit := 0
	for size >= 1024.0 && unit+1 < len(units) {
		size /= 1024.0
		unit++
	}
	if unit == 0 {
		return fmt.Sprintf("%d B", bytes)
	}
	return fmt.Sprintf("%.1f %s", size, units[unit])
}

// formatSuspendDryRun is what `mj suspend --dry-run` prints without
// `--json`.
func formatSuspendDryRun(preflight *core.SuspendPreflight) string {
	var out strings.Builder
	fmt.Fprintf(&out, "dry run for session %s\n", preflight.SessionID)
	if warning, ok := core.OwningChildrenSuspendWarning(preflight.OwningChildrenToSuspend); ok {
		out.WriteString(warning + "\n")
	}
	if estimate := preflight.DryRun; estimate != nil {
		switch {
		case estimate.GitDeltaBytes != nil:
			fmt.Fprintf(&out, "git delta: %s\n", formatHumanBytes(*estimate.GitDeltaBytes))
		case estimate.GitDeltaUnavailableReason != nil:
			fmt.Fprintf(&out, "git delta: %s\n", *estimate.GitDeltaUnavailableReason)
		default:
			out.WriteString("git delta: unavailable\n")
		}
		if estimate.SessionStateBytes != nil {
			fmt.Fprintf(&out, "session state: %s\n", formatHumanBytes(*estimate.SessionStateBytes))
		} else {
			out.WriteString("session state: unavailable\n")
		}
		// Suspend saves only Git state, so never present the keep
		// directory's size as kept here (#1645).
		if estimate.KeepDirBytes == 0 {
			out.WriteString(".mj/keep/: 0 B\n")
		} else {
			fmt.Fprintf(&out, ".mj/keep/: %s, not saved by suspend; copy out anything you need first, for example with `mj fetch`\n", formatHumanBytes(estimate.KeepDirBytes))
		}
		if estimate.TransferRoute != nil {
			fmt.Fprintf(&out, "transfer route: %s\n", *estimate.TransferRoute)
		}
	}
	if len(preflight.Blockers) > 0 {
		out.WriteString("blockers:\n")
		for _, blocker := range preflight.Blockers {
			fmt.Fprintf(&out, "  - %s: %s\n", blocker.Kind, blocker.Remedy)
		}
	}
	if len(preflight.Operations) > 0 {
		out.WriteString("in-progress operations (would be aborted):\n")
		for _, operation := range preflight.Operations {
			fmt.Fprintf(&out, "  - %s (%s)\n", operation.Kind, operation.Checkout.RepositoryID)
		}
	}
	if lost := preflight.UntrackedDisplayPaths(); len(lost) > 0 {
		out.WriteString("would be lost (ignored files are never kept):\n")
		for _, path := range lost {
			fmt.Fprintf(&out, "  - %s\n", path)
		}
	}
	return out.String()
}

// destroy permanently destroys the session, environment, and recovery
// archive.
func destroy(ctx context.Context, args destroyArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if err := api.destroy(ctx, args.session); err != nil {
		return err
	}
	if args.json {
		return printJSON(jsonvalue.Object{
			{Key: "session_id", Value: args.session},
			{Key: "accepted", Value: true},
			{Key: "operation", Value: "destroy"},
			{Key: "notices_url", Value: fmt.Sprintf("/api/v1/sessions/%s/notices", args.session)},
		})
	}
	fmt.Printf("destruction accepted for %s\n", args.session)
	fmt.Printf("it finishes in the background; check removal with `mj sessions --session %s`\n", args.session)
	fmt.Printf("read cleanup warnings with `mj notices --session %s`\n", args.session)
	return nil
}

// suspendSessionID is which session this suspension is for, or a message
// naming the option that says so.
func suspendSessionID(args *suspendArgs) (string, error) {
	switch {
	case args.session != nil && args.misplacedSession == nil:
		return *args.session, nil
	case args.session == nil && args.misplacedSession != nil:
		return "", fmt.Errorf("name the session as an option: `mj suspend --session %s`", *args.misplacedSession)
	case args.session != nil:
		return "", errors.New("name the session once, with --session")
	}
	return "", errors.New("name the session to suspend with --session <id>")
}

// resumeRequest is the request every resume sends with the person's
// choices.
func resumeRequest(args *resumeArgs) *server.ResumeSessionRequest {
	request := &server.ResumeSessionRequest{ProfileID: args.profile, TargetID: args.target, WorkspaceID: args.workspaceID}
	if args.queue != nil {
		disposition := args.queue.disposition()
		request.Queue = &disposition
	}
	return request
}

// resume resumes a suspended session from its checkpoint.
//
// The API answers as soon as the daemon admits the resume, because restoring
// an archive onto a fresh target takes minutes. Follow it with
// `mj wait --session <id>`, which blocks while the resume runs and reports
// why it failed if it does.
func resume(ctx context.Context, args resumeArgs) error {
	if args.wiki != nil {
		return resumeWiki(ctx, args, *args.wiki)
	}
	if args.session == nil {
		return errors.New("name the session to resume with --session <id>")
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	response, err := resumeWithWarnings(ctx, api, *args.session, resumeRequest(&args))
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(response)
	}
	fmt.Printf("resuming %s on profile %s and target %s\n", response.SessionID, response.ProfileID, response.TargetID)
	fmt.Printf("watch it with `mj wait --session %s`\n", response.SessionID)
	return nil
}

// resumeWithWarnings resumes a session and prints the launch notices the
// daemon produced meanwhile.
func resumeWithWarnings(ctx context.Context, api *apiClient, sessionID string, request *server.ResumeSessionRequest) (server.ResumeSessionResponse, error) {
	warnings := newLaunchWarnings(ctx)
	defer warnings.close()
	response, err := api.resume(ctx, sessionID, request)
	if err != nil {
		return server.ResumeSessionResponse{}, err
	}
	warnings.report(ctx, sessionID)
	return response, nil
}

// resumeWiki continues the session a SessionWiki id names, whoever ran it.
//
// The branch is the daemon's: a Mjolnir session Mjolnir still has a record
// of is resumed, one it has archived is restored from the indexed
// transcript, and another tool's session is imported and then resumed. The
// output says which of the three ran, so the caller can follow with
// `mj prompt` or `mj message`.
func resumeWiki(ctx context.Context, args resumeArgs, wikiID string) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	info, err := api.wikiSession(ctx, wikiID)
	if err != nil {
		return err
	}
	if info == nil {
		return fmt.Errorf("no indexed session %s", wikiID)
	}
	continuation, err := sessionwiki.WikiContinuationOf(info.WikiID, info.Tool, info.Path, info.Status == client.WikiSessionStatusMine)
	if err != nil {
		return err
	}
	switch continuation := continuation.(type) {
	case sessionwiki.WikiContinuationResume:
		response, err := resumeWithWarnings(ctx, api, continuation.SessionID, resumeRequest(&args))
		if err != nil {
			return err
		}
		return reportWikiContinuation(&args, "resume", wikiID, response.SessionID, func() string {
			return "resumed " + response.SessionID
		})
	case sessionwiki.WikiContinuationRestore:
		restoring := continuation.WikiID
		profileID := args.profile
		if profileID == nil {
			profileID = info.ProfileID
		}
		if profileID == nil {
			return fmt.Errorf("the indexed session %s records no profile; name one with --profile", restoring)
		}
		targetID := args.target
		if targetID == nil {
			targetID = info.TargetTemplateID
		}
		if targetID == nil {
			return fmt.Errorf("the indexed session %s records no target; name one with --target", restoring)
		}
		warnings := newLaunchWarnings(ctx)
		defer warnings.close()
		response, err := api.wikiRestore(ctx, restoring, &server.WikiRestoreBody{
			WorkspaceID: args.workspaceID,
			ProfileID:   *profileID,
			TargetID:    *targetID,
		})
		if err != nil {
			return err
		}
		warnings.report(ctx, response.SessionID)
		return reportWikiContinuation(&args, "restore", wikiID, response.SessionID, func() string {
			return fmt.Sprintf("restored %s into %s", restoring, response.SessionID)
		})
	case sessionwiki.WikiContinuationImport:
		if args.workspaceID == nil {
			if _, err := resolveStoreWorkspace(ctx, args.workspace.name); err != nil {
				return err
			}
		}
		// The import is import.rs's import_named_native_session, which the
		// Go build does not have yet.
		return fmt.Errorf("import the %s session %s, which the index read from %s: %w", info.Tool, continuation.NativeSessionID, info.Path, errNotPorted)
	}
	panic(fmt.Sprintf("unknown wiki continuation %T", continuation))
}

// reportWikiContinuation is one line saying which of the three cases ran, or
// the same three facts as JSON. The `mj wait` hint is the same one a plain
// resume prints.
func reportWikiContinuation(args *resumeArgs, action, wikiID, sessionID string, summary func() string) error {
	if args.json {
		return printJSON(jsonvalue.Object{
			{Key: "action", Value: action},
			{Key: "session_id", Value: sessionID},
			{Key: "wiki_id", Value: wikiID},
		})
	}
	fmt.Println(summary())
	fmt.Printf("watch it with `mj wait --session %s`\n", sessionID)
	return nil
}

// interruptTurn cancels the turn a session is running.
func interruptTurn(ctx context.Context, args sessionArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if err := api.interruptTurn(ctx, args.session); err != nil {
		return err
	}
	if args.json {
		return printJSON(jsonvalue.Object{{Key: "session_id", Value: args.session}, {Key: "accepted", Value: true}})
	}
	fmt.Printf("cancelling the turn on %s\n", args.session)
	return nil
}

// restartSession restarts a running session's agent.
func restartSession(ctx context.Context, args sessionArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	payload, err := api.restartSession(ctx, args.session)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(payload)
	}
	fmt.Printf("restarting agent for %s\n", args.session)
	return nil
}

// reviewArgs are the arguments of `mj review`.
type reviewArgs struct {
	command reviewCommand
}

// bind declares the review subcommands on cmd; run runs the parsed command.
func (args *reviewArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	var start sessionArgs
	startCmd := subcommand("start", "Review the turn the session just finished, with the session's reviewer. The session must be idle with no queued prompts")
	meta.subcommand(startCmd)
	start.bind(startCmd, func() error {
		args.command = reviewCommandStart{start}
		return run()
	})
	var status sessionArgs
	statusCmd := subcommand("status", "Show the review the session has open: what it is doing, each reviewing role, and its verdict once it has one")
	meta.subcommand(statusCmd)
	status.bind(statusCmd, func() error {
		args.command = reviewCommandStatus{status}
		return run()
	})
	var show reviewShowArgs
	showCmd := subcommand("show", "Show a finished review's per-reviewer output: each reviewer's model, effort and profile, its full handback, its verdict, and its usage. Works after the reviewer sessions have been destroyed")
	meta.subcommand(showCmd)
	show.bind(showCmd, func() error {
		args.command = reviewCommandShow{show}
		return run()
	})
	group(cmd, true, nil)
}

// reviewCommand is a `mj review` subcommand.
type reviewCommand interface{ isReviewCommand() }

// reviewCommandStart reviews the turn the session just finished, with the
// session's reviewer. The session must be idle with no queued prompts.
type reviewCommandStart struct{ args sessionArgs }

// reviewCommandStatus shows the review the session has open: what it is
// doing, each reviewing role, and its verdict once it has one.
type reviewCommandStatus struct{ args sessionArgs }

// reviewCommandShow shows a finished review's per-reviewer output: each
// reviewer's model, effort and profile, its full handback, its verdict, and
// its usage. Works after the reviewer sessions have been destroyed.
type reviewCommandShow struct{ args reviewShowArgs }

func (reviewCommandStart) isReviewCommand()  {}
func (reviewCommandStatus) isReviewCommand() {}
func (reviewCommandShow) isReviewCommand()   {}

// reviewShowArgs are the arguments of `mj review show`.
type reviewShowArgs struct {
	session sessionArgs
	// review is the review id, as a verdict message names it. Defaults to
	// the newest finished review for the session.
	review *string
}

// bind declares the review show options on cmd.
func (args *reviewShowArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	args.session.declare(meta)
	meta.option("review", "Review id, as a verdict message names it. Defaults to the newest finished review for the session", storeOptional(&args.review))
	leaf(cmd, run)
}

// review reviews a session's committed work, or shows the review it has
// open.
func review(ctx context.Context, args reviewArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	switch command := args.command.(type) {
	case reviewCommandStart:
		started, err := api.startReview(ctx, command.args.session)
		if err != nil {
			return err
		}
		if command.args.json {
			return printJSON(started)
		}
		fmt.Printf("review started on %s\n", command.args.session)
		fmt.Printf("follow it with `mj review status --session %s`\n", command.args.session)
		return nil
	case reviewCommandStatus:
		status, err := api.reviewStatus(ctx, command.args.session)
		if err != nil {
			return err
		}
		if command.args.json {
			return printJSON(status)
		}
		fmt.Printf("Review trigger: %s\n", status.Trigger)
		printLines(reviewLines(status.Review))
		return nil
	case reviewCommandShow:
		response, err := api.finishedReview(ctx, command.args.session.session, command.args.review)
		if err != nil {
			return err
		}
		if command.args.session.json {
			return printJSON(response)
		}
		printLines(finishedReviewLines(&response))
		return nil
	}
	panic(fmt.Sprintf("unknown review command %T", args.command))
}

// reviewLines is a review as plain lines: its status, one line per role,
// then the verdict and its text.
func reviewLines(review *server.ViewerTurnReview) []string {
	if review == nil {
		return []string{"no review is open"}
	}
	lines := []string{"review: " + review.Status}
	for _, role := range review.Roles {
		lines = append(lines, fmt.Sprintf("  %s: %s", role.Label, role.State))
	}
	if verdict := review.Verdict; verdict != nil {
		lines = append(lines, "verdict: "+verdict.Kind)
		if verdict.Text != "" {
			lines = append(lines, "", verdict.Text)
		}
	}
	return lines
}

// finishedReviewLines is a finished review as plain lines, including each
// reviewer's full handback and the usage its deleted child session still
// accounts for.
func finishedReviewLines(response *server.FinishedReviewResponse) []string {
	review := response.Review
	if review == nil {
		return []string{"no finished review is recorded for " + response.SessionID}
	}
	resolution := map[core.FinishedReviewResolution]string{
		core.FinishedReviewResolutionCompleted: "completed",
		core.FinishedReviewResolutionFailed:    "failed",
		core.FinishedReviewResolutionCancelled: "cancelled",
	}[review.Resolution]
	finished := fmt.Sprintf("%d ms", review.FinishedAtMs)
	// chrono's DateTime::from_timestamp_millis covers years -262143 to
	// 262142.
	if milliseconds := review.FinishedAtMs; milliseconds >= -8334601228800000 && milliseconds <= 8210266876799999 {
		finished = time.UnixMilli(milliseconds).UTC().Format("2006-01-02T15:04:05Z")
	}
	lines := []string{fmt.Sprintf("review %s: %s, finished %s", review.ReviewID, resolution, finished)}
	for _, role := range review.Roles {
		profile, model, effort := role.Profile, "harness default", "default effort"
		if profile == "" {
			profile = "unknown"
		}
		if role.Model != nil {
			model = *role.Model
		}
		if role.Effort != nil {
			effort = *role.Effort
		}
		lines = append(lines, fmt.Sprintf("  %s (%s) profile %s; model %s; effort %s", role.Label, role.Role, profile, model, effort))
		switch verdict := role.Verdict.(type) {
		case core.ReviewVerdictClean:
			lines = append(lines, "    verdict: clean")
		case core.ReviewVerdictFindings:
			lines = append(lines, "    verdict: findings")
			for _, line := range rustLines(verdict.Synthesis) {
				lines = append(lines, "      "+line)
			}
		case core.ReviewVerdictFailed:
			lines = append(lines, "    verdict: failed: "+verdict.Reason)
		case nil:
			lines = append(lines, "    verdict: none (the reviewer did not settle)")
		}
		if turn := role.Turn; turn != nil {
			result := turn.Result()
			kind := map[core.TurnResultKind]string{
				core.TurnResultKindCompleted:     "completed",
				core.TurnResultKindInputRequired: "input required",
				core.TurnResultKindCancelled:     "cancelled",
				core.TurnResultKindRejected:      "rejected",
				core.TurnResultKindInterrupted:   "interrupted",
				core.TurnResultKindFailed:        "failed",
			}[result.Kind]
			detail := ""
			if result.Message != nil {
				detail = ": " + *result.Message
			} else if result.StopReason != nil {
				detail = ": " + *result.StopReason
			}
			lines = append(lines, fmt.Sprintf("    turn: %s%s", kind, detail))
			if usage := turn.Usage; usage != nil {
				counters := []string{
					fmt.Sprintf("total %d", usage.TotalTokens),
					fmt.Sprintf("input %d", usage.InputTokens),
					fmt.Sprintf("output %d", usage.OutputTokens),
				}
				for _, counter := range []struct {
					name  string
					value *uint64
				}{{"thought", usage.ThoughtTokens}, {"cached read", usage.CachedReadTokens}, {"cached write", usage.CachedWriteTokens}} {
					if counter.value != nil {
						counters = append(counters, fmt.Sprintf("%s %d", counter.name, *counter.value))
					}
				}
				lines = append(lines, "    usage: "+strings.Join(counters, ", "))
			}
		}
		if handback := role.Handback; handback != nil {
			lines = append(lines, "    handback:")
			if *handback == "" {
				lines = append(lines, "      (empty)")
			}
			for _, line := range rustLines(*handback) {
				lines = append(lines, "      "+line)
			}
		}
	}
	return lines
}

// apiInfo prints the API location without exposing credentials.
func apiInfo(ctx context.Context, args apiInfoArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if args.json {
		return printJSON(jsonvalue.Object{{Key: "base_url", Value: api.baseURL() + "/api/v1"}})
	}
	fmt.Printf("base url   %s/api/v1\n", api.baseURL())
	fmt.Println("version    Mjolnir API 1")
	return nil
}

// apiToken is explicit credential output for callers authenticating outside
// the CLI.
func apiToken(ctx context.Context) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	// The daemon sets the credential before it publishes viewer readiness;
	// a daemon this command just started may not have reached that point.
	if _, err := WaitForWebAccess(ctx, daemon); err != nil {
		return err
	}
	credential, err := daemon.APICredential(ctx)
	if err != nil {
		return err
	}
	fmt.Println(credential)
	return nil
}

// githubToken prints a valid GitHub App installation token for a selected
// owner or repo.
func githubToken(ctx context.Context, args githubTokenArgs) error {
	var owner *string
	var repositories []string
	switch {
	case args.owner != nil && len(args.repo) == 0:
		owner, repositories = args.owner, []string{}
	case args.owner == nil && len(args.repo) > 0:
		for _, repository := range args.repo {
			parsedOwner, parsedName, ok := core.GithubOwnerRepo(repository)
			if !ok {
				return errors.New("--repo values must use OWNER/NAME form")
			}
			if parsedOwner+"/"+parsedName != repository {
				return errors.New("--repo values must use OWNER/NAME form")
			}
		}
		repositories = args.repo
	default:
		return errors.New("supply exactly one of --owner or one or more --repo values")
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	token, err := api.githubToken(ctx, owner, repositories)
	if err != nil {
		return err
	}
	fmt.Println(token)
	return nil
}

// readPrompt reads prompt text from an argument, a file, or standard input.
func readPrompt(text *string, file *string) (*string, error) {
	switch {
	case text != nil && file != nil:
		return nil, errors.New("pass the prompt text or --prompt-file, not both")
	case text != nil && *text == "-":
		read, err := readStdin()
		if err != nil {
			return nil, err
		}
		return &read, nil
	case text != nil:
		return text, nil
	case file != nil && *file == "-":
		read, err := readStdin()
		if err != nil {
			return nil, err
		}
		return &read, nil
	case file != nil:
		read, err := os.ReadFile(*file)
		if err != nil {
			return nil, fmt.Errorf("read prompt file %s: %w", *file, err)
		}
		if !utf8.Valid(read) {
			return nil, fmt.Errorf("read prompt file %s: stream did not contain valid UTF-8", *file)
		}
		text := string(read)
		return &text, nil
	}
	return nil, nil
}

// readStdin reads the prompt from standard input, refusing a terminal.
func readStdin() (string, error) {
	if term.IsTerminal(os.Stdin.Fd()) {
		return "", errors.New("`-` reads the prompt from standard input, but standard input is a terminal")
	}
	read, err := io.ReadAll(os.Stdin)
	if err != nil {
		return "", fmt.Errorf("read the prompt from standard input: %w", err)
	}
	if !utf8.Valid(read) {
		return "", errors.New("read the prompt from standard input: stream did not contain valid UTF-8")
	}
	return string(read), nil
}

// writeBytes writes an export to out, or to standard output when no file is
// named. With `--json` and a file, the confirmation is one JSON object
// rather than a sentence; without a file the bytes are the output and stay
// unwrapped.
func writeBytes(bytes []byte, out *string, json bool, format string) error {
	return writeBytesTo(os.Stdout, bytes, out, json, format)
}

// writeBytesTo is writeBytes with standard output given.
func writeBytesTo(stdout io.Writer, bytes []byte, out *string, json bool, format string) error {
	if out == nil {
		if _, err := stdout.Write(bytes); err != nil {
			return fmt.Errorf("write the export: %w", err)
		}
		return nil
	}
	if err := os.WriteFile(*out, bytes, 0o666); err != nil {
		return fmt.Errorf("write %s: %w", *out, err)
	}
	if json {
		report, err := core.SerdeJSONPretty(jsonvalue.Object{
			{Key: "path", Value: *out},
			{Key: "bytes", Value: len(bytes)},
			{Key: "format", Value: format},
		})
		if err != nil {
			return err
		}
		if _, err := fmt.Fprintln(stdout, string(report)); err != nil {
			return fmt.Errorf("report the export: %w", err)
		}
		return nil
	}
	if _, err := fmt.Fprintf(stdout, "wrote %d bytes to %s\n", len(bytes), *out); err != nil {
		return fmt.Errorf("report the export: %w", err)
	}
	return nil
}

// printLines prints report lines built from daemon answers. Titles, notices
// and other text in them can come from an agent, so terminal controls are
// removed here, where that text reaches the operator's terminal.
func printLines(lines []string) {
	for _, line := range lines {
		fmt.Println(core.SanitizeTerminalText(line))
	}
}

// printJSON prints value as serde's pretty JSON.
func printJSON(value any) error {
	encoded, err := core.SerdeJSONPretty(value)
	if err != nil {
		return fmt.Errorf("serialize the API response: %w", err)
	}
	fmt.Println(string(encoded))
	return nil
}

// modelsArgs are the arguments of `mj models`.
type modelsArgs struct {
	// profile is the profile whose harness to ask. Omitted lists the models
	// every enabled profile offers, with the profiles supporting each.
	profile *string
	// model lists the efforts this model offers instead of the default
	// model's. Requires profile.
	model *string
	// unrestricted lists every model the harness advertises, ignoring the
	// profile's `models` allow-list. Use it to find the exact values to put
	// there. Conflicts with model.
	unrestricted bool
	// withDisabled, with no --profile, includes disabled profiles in the
	// aggregate.
	withDisabled bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the models options on cmd.
func (args *modelsArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("profile", "Profile whose harness to ask. Omitted lists the models every enabled profile offers, with the profiles supporting each", storeOptional(&args.profile))
	meta.option("model", "List the efforts this model offers instead of the default model's", storeOptional(&args.model), requires("profile"))
	meta.flag(&args.unrestricted, "unrestricted", "List every model the harness advertises, ignoring the profile's `models` allow-list. Use it to find the exact values to put there", conflictsWith("model"))
	meta.flag(&args.withDisabled, "with_disabled", "With no --profile, include disabled profiles in the aggregate")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// setConfigArgs are the arguments of `mj set-config`.
type setConfigArgs struct {
	// session is the session id, as `mj sessions` lists it.
	session string
	// key is the setting to change, such as `model` or `effort`. Omit it and
	// --value to list the settings this session's agent offers, with their
	// choices. Requires value.
	key *string
	// value is the value to set: one of the choices the setting lists.
	// Requires key.
	value *string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the set-config options on cmd.
func (args *setConfigArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("session", sessionIDHelp, storeString(&args.session), required())
	meta.option("key", "Setting to change, such as `model` or `effort`. Omit it and --value to list the settings this session's agent offers, with their choices", storeOptional(&args.key), requires("value"))
	meta.option("value", "Value to set: one of the choices the setting lists", storeOptional(&args.value), requires("key"))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// models discovers available models and efforts for a profile, or for every
// enabled profile.
func models(ctx context.Context, args modelsArgs) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	frame, err := daemon.RuntimeChanges(ctx, nil, false)
	daemon.Close()
	if err != nil {
		return err
	}
	if frame.Kind != client.RuntimeFrameSnapshot || frame.Projection == nil {
		return errors.New("daemon did not return a runtime snapshot for cached quota reports")
	}
	metadata := &frame.Projection.Metadata
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	if args.profile == nil {
		config := metadata.InstalledConfig()
		var ids []string
		for _, id := range sortedKeys(config.Profiles) {
			if args.withDisabled || config.Profiles[id].Enabled {
				ids = append(ids, id)
			}
		}
		results := make([]profileModels, len(ids))
		var asking sync.WaitGroup
		for index, id := range ids {
			asking.Go(func() {
				config, err := api.models(ctx, id, nil, args.unrestricted, args.withDisabled)
				results[index] = profileModels{profile: id, config: config, err: err}
			})
		}
		asking.Wait()
		rendered, err := renderModelsAggregate(results, args.json)
		if err != nil {
			return err
		}
		fmt.Println(rendered)
		return nil
	}
	billing := client.QuotaBillingUnknown
	if report, ok := metadata.Quotas.Get().Reports[*args.profile]; ok {
		billing = report.BillingClass()
	}
	choices, err := api.models(ctx, *args.profile, args.model, args.unrestricted, args.withDisabled)
	if err != nil {
		return err
	}
	rendered, err := renderModels(&choices, billing, args.json)
	if err != nil {
		return err
	}
	fmt.Println(rendered)
	return nil
}

// profileModels is one profile's answer to a models request, as the
// aggregate collects them (Rust's (String, Result<ProfileConfig>)).
type profileModels struct {
	profile string
	config  core.ProfileConfig
	err     error
}

// renderModelsAggregate is the aggregated `mj models` view: one row per
// normalized model identity, naming every profile that offers it. Provider
// qualifiers fold away, so `openai/gpt-5.3-codex` and `gpt-5.3-codex` share a
// row.
func renderModelsAggregate(results []profileModels, json bool) (string, error) {
	type offered struct {
		name     string
		profiles []string
	}
	byModel := map[string]*offered{}
	unavailable := map[string]string{}
	var unavailableOrder []string
	for _, result := range results {
		if result.err != nil {
			unavailable[result.profile] = result.err.Error()
			unavailableOrder = append(unavailableOrder, result.profile)
			continue
		}
		for _, model := range result.config.Models {
			key := core.NormalizeModel(model.Value)
			entry, ok := byModel[key]
			if !ok {
				entry = &offered{name: model.Name}
				byModel[key] = entry
			}
			entry.profiles = append(entry.profiles, result.profile)
		}
	}
	values := sortedKeys(byModel)
	if json {
		models := make([]any, 0, len(values))
		for _, value := range values {
			entry := byModel[value]
			profiles := make([]any, 0, len(entry.profiles))
			for _, profile := range entry.profiles {
				profiles = append(profiles, profile)
			}
			models = append(models, jsonvalue.Object{
				{Key: "value", Value: value},
				{Key: "name", Value: entry.name},
				{Key: "profiles", Value: profiles},
			})
		}
		// A BTreeMap of the failures: its keys sorted.
		failures := jsonvalue.Object{}
		for _, profile := range sortedKeys(unavailable) {
			failures = append(failures, jsonvalue.Member{Key: profile, Value: unavailable[profile]})
		}
		encoded, err := core.SerdeJSONPretty(jsonvalue.Object{{Key: "models", Value: models}, {Key: "unavailable", Value: failures}})
		if err != nil {
			return "", fmt.Errorf("serialize the aggregated model catalog: %w", err)
		}
		return string(encoded), nil
	}
	var lines []string
	for _, value := range values {
		entry := byModel[value]
		lines = append(lines, fmt.Sprintf("%s  %s: %s", value, entry.name, strings.Join(entry.profiles, ", ")))
	}
	if len(byModel) == 0 {
		lines = append(lines, "no profile reports any models")
	}
	for _, profile := range unavailableOrder {
		lines = append(lines, fmt.Sprintf("%s: %s", profile, unavailable[profile]))
	}
	return strings.Join(lines, "\n"), nil
}

// renderModels is one profile's models and efforts, with prices when the
// profile bills by usage.
func renderModels(choices *core.ProfileConfig, billing client.QuotaBilling, json bool) (string, error) {
	rates := func(model string) ([4]uint64, bool) {
		if billing != client.QuotaBillingUsage {
			return [4]uint64{}, false
		}
		return core.Rates(model)
	}
	if json {
		value, err := toValueObject(choices)
		if err != nil {
			return "", fmt.Errorf("serialize model catalog: %w", err)
		}
		value.Set("billing", map[client.QuotaBilling]string{
			client.QuotaBillingUsage:        "usage",
			client.QuotaBillingSubscription: "subscription",
			client.QuotaBillingUnknown:      "unknown",
		}[billing])
		models := make([]any, 0, len(choices.Models))
		for _, model := range choices.Models {
			item, err := toValueObject(model)
			if err != nil {
				return "", fmt.Errorf("model choice serializes: %w", err)
			}
			var price any
			if r, ok := rates(model.Value); ok {
				price = jsonvalue.Object{
					{Key: "input", Value: float64(r[0]) / 1000.0},
					{Key: "output", Value: float64(r[1]) / 1000.0},
					{Key: "cached_input", Value: float64(r[2]) / 1000.0},
					{Key: "cache_write", Value: float64(r[3]) / 1000.0},
				}
			}
			item.Set("price", price)
			models = append(models, item)
		}
		value.Set("models", models)
		pretty, err := core.SerdeJSONPretty(value)
		if err != nil {
			return "", fmt.Errorf("serialize model catalog: %w", err)
		}
		return string(pretty), nil
	}
	var lines []string
	if billing != client.QuotaBillingUsage {
		name := "unknown"
		if billing == client.QuotaBillingSubscription {
			name = "subscription"
		}
		lines = append(lines, "billing: "+name)
	}
	if len(choices.Models) == 0 && len(choices.Efforts) == 0 {
		lines = append(lines, "this profile's harness reports no models or efforts")
	}
	for _, model := range choices.Models {
		price := ""
		if r, ok := rates(model.Value); ok {
			price = fmt.Sprintf("  $%.3f in / $%.3f cached / $%.3f out per 1M", float64(r[0])/1000.0, float64(r[2])/1000.0, float64(r[1])/1000.0)
		}
		lines = append(lines, fmt.Sprintf("%s  %s%s", model.Value, model.Name, price))
	}
	// A harness without efforts, or one whose efforts are not known yet,
	// printed a bare "effort (default):" line, which reads as a value that
	// failed to print (F-16).
	if len(choices.Efforts) > 0 {
		model := "default model"
		if choices.Model != nil {
			model = *choices.Model
		}
		efforts := make([]string, 0, len(choices.Efforts))
		for _, effort := range choices.Efforts {
			efforts = append(efforts, effort.Value)
		}
		lines = append(lines, fmt.Sprintf("effort (%s): %s", model, strings.Join(efforts, ", ")))
	}
	return strings.Join(lines, "\n"), nil
}

// toValueObject is serde_json::to_value of a struct: the object serde
// writes, members in field order, to change before it is printed.
func toValueObject(value any) (jsonvalue.Object, error) {
	encoded, err := core.SerdeJSON(value)
	if err != nil {
		return nil, err
	}
	parsed, err := jsonvalue.Parse(encoded)
	if err != nil {
		return nil, err
	}
	object, ok := parsed.(jsonvalue.Object)
	if !ok {
		return nil, fmt.Errorf("%T does not serialize as an object", value)
	}
	return object, nil
}

// quotaArgs are the arguments of `mj quota`.
type quotaArgs struct {
	// profile shows only this profile; default lists every enabled profile.
	profile *string
	// refresh asks the daemon to probe again, and waits for the fresh
	// cycle's readings.
	refresh bool
	// withDisabled includes disabled profiles, probing once for any without
	// a report.
	withDisabled bool
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the quota options on cmd.
func (args *quotaArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("profile", "Show only this profile; default lists every enabled profile", storeOptional(&args.profile))
	meta.flag(&args.refresh, "refresh", "Ask the daemon to probe again, and wait for the fresh cycle's readings")
	meta.flag(&args.withDisabled, "with_disabled", "Include disabled profiles, probing once for any without a report")
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// quotaRefreshWait is how long `mj quota --refresh` waits for the cycle it
// asked for before printing what the daemon already has. Quota probes launch
// each profile's agent, so a fresh cycle can take a while on a loaded host.
const quotaRefreshWait = 120 * time.Second

// pumpQuotaFeed pumps runtime deltas until done holds or the deadline
// passes; it returns the latest cursor and metadata seen. One waiter feeds
// `--refresh` (a new cycle) and `--with-disabled` (a report for every
// profile just probed). It owns a dedicated connection: a deadline abandons
// its in-flight exchange, while quota control requests continue on their
// original connection.
func pumpQuotaFeed(
	ctx context.Context,
	daemon *client.DaemonClient,
	cursor *client.RuntimeCursor,
	metadata client.RuntimeMetadata,
	deadline time.Time,
	done func(*client.RuntimeMetadata) bool,
) (*client.RuntimeCursor, client.RuntimeMetadata, error) {
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	return pumpQuotaFeedUntil(ctx, daemon, cursor, metadata, timer.C, done)
}

// pumpQuotaFeedUntil is pumpQuotaFeed with its deadline as the timer that
// fires at it (tokio's timeout over the remaining time). The exchange in
// flight when it fires is abandoned, which drops the feed's connection.
func pumpQuotaFeedUntil(
	ctx context.Context,
	daemon *client.DaemonClient,
	cursor *client.RuntimeCursor,
	metadata client.RuntimeMetadata,
	expired <-chan time.Time,
	done func(*client.RuntimeMetadata) bool,
) (*client.RuntimeCursor, client.RuntimeMetadata, error) {
	defer daemon.Close()
	type answer struct {
		frame client.RuntimeFrame
		err   error
	}
	for !done(&metadata) {
		select {
		case <-expired:
			return cursor, metadata, nil
		default:
		}
		exchange, cancel := context.WithCancel(ctx)
		answered := make(chan answer, 1)
		go func(cursor *client.RuntimeCursor) {
			frame, err := daemon.RuntimeChanges(exchange, cursor, true)
			answered <- answer{frame, err}
		}(cursor)
		var result answer
		select {
		case result = <-answered:
			cancel()
		case <-expired:
			// Cancelling drops the connection, which ends the exchange.
			cancel()
			<-answered
			return cursor, metadata, nil
		}
		if result.err != nil {
			return nil, client.RuntimeMetadata{}, result.err
		}
		switch result.frame.Kind {
		case client.RuntimeFrameSnapshot:
			next := result.frame.Cursor
			cursor = &next
			if result.frame.Projection != nil {
				metadata = result.frame.Projection.Metadata
			}
		case client.RuntimeFrameDelta:
			next := result.frame.Cursor
			cursor = &next
			if result.frame.Changes != nil && result.frame.Changes.Metadata != nil {
				metadata = *result.frame.Changes.Metadata
			}
		case client.RuntimeFrameResetRequired:
			cursor = nil
		}
	}
	return cursor, metadata, nil
}

// quota shows each profile's remaining quota as the daemon last read it.
func quota(ctx context.Context, args quotaArgs) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	frame, err := daemon.RuntimeChanges(ctx, nil, false)
	if err != nil {
		return err
	}
	if frame.Kind != client.RuntimeFrameSnapshot || frame.Projection == nil {
		return errors.New("daemon did not return a runtime snapshot for quota reports")
	}
	first := frame.Cursor
	cursor := &first
	metadata := frame.Projection.Metadata
	if args.refresh {
		// The daemon coalesces this with any probe already running; the
		// cycle count advancing past the request's is the refresh done.
		pending := metadata.Quotas.Get().Cycles
		if err := daemon.RefreshQuota(ctx); err != nil {
			return err
		}
		feed, err := client.ConnectExisting(ctx)
		if err != nil {
			return err
		}
		if cursor, metadata, err = pumpQuotaFeed(ctx, feed, cursor, metadata, time.Now().Add(quotaRefreshWait),
			func(metadata *client.RuntimeMetadata) bool { return metadata.Quotas.Get().Cycles > pending }); err != nil {
			return err
		}
	}
	if args.withDisabled {
		config := metadata.InstalledConfig()
		// A disabled profile with a served report shows it; one without gets
		// a single daemon probe, and this waits for the answer.
		var probe []string
		for _, id := range sortedKeys(config.Profiles) {
			if _, reported := metadata.Quotas.Get().Reports[id]; !config.Profiles[id].Enabled &&
				(args.profile == nil || *args.profile == id) && !reported {
				probe = append(probe, id)
			}
		}
		if len(probe) > 0 {
			if err := daemon.ProbeQuotaProfiles(ctx, probe, false); err != nil {
				return err
			}
			feed, err := client.ConnectExisting(ctx)
			if err != nil {
				return err
			}
			if _, metadata, err = pumpQuotaFeed(ctx, feed, cursor, metadata, time.Now().Add(quotaRefreshWait),
				func(metadata *client.RuntimeMetadata) bool {
					for _, id := range probe {
						if _, ok := metadata.Quotas.Get().Reports[id]; !ok {
							return false
						}
					}
					return true
				}); err != nil {
				return err
			}
		}
	}
	rendered, err := renderQuota(&metadata, args.profile, args.withDisabled, args.json)
	if err != nil {
		return err
	}
	fmt.Println(rendered)
	return nil
}

// quotaRow is one profile's row for `mj quota`, or the reason its quota
// cannot be read: the text line and the JSON entry (an object whose members
// keep the order they were inserted in, as mj's serde_json Value does with
// `preserve_order`).
func quotaRow(id string, profile *core.HarnessProfile, snapshot *client.QuotaSnapshot, now uint64) (string, jsonvalue.Object) {
	var report *client.ProfileQuota
	if found, ok := snapshot.Reports[id]; ok {
		report = &found
	}
	billing := client.ProfileQuotaBillingFor(profile.Kind, report)
	billingLabel := map[client.QuotaBilling]string{
		client.QuotaBillingUsage:        "usage",
		client.QuotaBillingSubscription: "subscription",
		client.QuotaBillingUnknown:      "unknown",
	}[billing]
	_, probing := snapshot.Probing[id]
	var encodedReport any
	if report != nil {
		encodedReport = report
	}
	entry := jsonvalue.Object{
		{Key: "profile", Value: id},
		{Key: "harness", Value: profile.Kind.ID()},
		{Key: "billing", Value: billingLabel},
		{Key: "enabled", Value: profile.Enabled},
		{Key: "probing", Value: probing},
		{Key: "report", Value: encodedReport},
	}
	// The quota text, and the reason a missing or failed reading shows.
	var status string
	switch {
	case report == nil && probing:
		status = "probing now"
	case report == nil:
		status = "no quota report yet"
	default:
		if label, ok := report.ErrorLabel(); ok {
			held := ""
			if limit, ok := report.RateLimitLabel(now); ok {
				held = fmt.Sprintf(" (%s)", limit)
			}
			status = fmt.Sprintf("error: %s%s", label, held)
		} else if label, ok := report.WindowlessLabel(); ok {
			status = label
		} else {
			parts := make([]string, 0, len(report.Windows))
			for index := range report.Windows {
				window := &report.Windows[index]
				var usage string
				switch {
				case window.RemainingPercent != nil:
					usage = fmt.Sprintf("%d%% left", *window.RemainingPercent)
				case window.Used != nil && window.Limit != nil:
					usage = fmt.Sprintf("%d/%d used", *window.Used, *window.Limit)
				default:
					usage = "available"
				}
				reset := window.ResetDisplay(now, report.BankedResetsForWindow(window))
				label := report.WindowLabel(window)
				if reset == "" {
					parts = append(parts, fmt.Sprintf("%s %s", label, usage))
				} else {
					parts = append(parts, fmt.Sprintf("%s %s, resets %s", label, usage, reset))
				}
			}
			status = strings.Join(parts, " · ")
		}
	}
	credits := ""
	if report != nil && report.PaidCredits != nil {
		switch report.PaidCredits.Kind {
		case client.PaidCreditsLimited:
			balance, err := rustNumber(report.PaidCredits.Balance)
			if err != nil {
				// A balance decoded from the daemon's JSON always encodes;
				// show what Go holds rather than hide the row.
				balance = fmt.Sprint(report.PaidCredits.Balance)
			}
			credits = fmt.Sprintf("credits $%s left", balance)
		case client.PaidCreditsUnlimited:
			credits = "unlimited credits"
		case client.PaidCreditsAvailable:
			credits = "credits available"
		}
	}
	var parts []string
	for _, part := range []string{status, credits} {
		if part != "" {
			parts = append(parts, part)
		}
	}
	status = strings.Join(parts, " · ")
	entry.Set("detail", status)
	disabled := ""
	if !profile.Enabled {
		disabled = ", disabled"
	}
	text := fmt.Sprintf("%s (%s, %s%s): %s", id, profile.Kind.DisplayName(), billingLabel, disabled, status)
	if report != nil {
		text += " · checked " + quotaAge(now, report.RefreshedAtEpochSeconds) + " ago"
	}
	return text, entry
}

// rustNumber is a serde_json::Number as its Display writes it, which is how
// serde_json writes the number in JSON: an integer as one, a float in its
// shortest round-trip form.
func rustNumber(value any) (string, error) {
	encoded, err := jsonvalue.Marshal(value)
	if err != nil {
		return "", err
	}
	return string(encoded), nil
}

// quotaAge is a compact age for a reading: `45s`, `4m`, `2h`, `3d`.
func quotaAge(now, epochSeconds uint64) string {
	var age uint64
	if now > epochSeconds {
		age = now - epochSeconds
	}
	switch {
	case age < 60:
		return fmt.Sprintf("%ds", age)
	case age < 3600:
		return fmt.Sprintf("%dm", age/60)
	case age < 24*3600:
		return fmt.Sprintf("%dh", age/3600)
	}
	return fmt.Sprintf("%dd", age/(24*3600))
}

// renderQuota is renderQuotaAt now.
func renderQuota(metadata *client.RuntimeMetadata, only *string, withDisabled, json bool) (string, error) {
	return renderQuotaAt(metadata, only, withDisabled, json, core.EpochSeconds())
}

// renderQuotaAt is what `mj quota` prints at now.
func renderQuotaAt(metadata *client.RuntimeMetadata, only *string, withDisabled, json bool, now uint64) (string, error) {
	config := metadata.InstalledConfig()
	snapshot := metadata.Quotas.Get()
	type row struct {
		text  string
		entry jsonvalue.Object
	}
	var rows []row
	for _, id := range sortedKeys(config.Profiles) {
		profile := config.Profiles[id]
		if !profile.Enabled && !withDisabled {
			continue
		}
		if only != nil && *only != id {
			continue
		}
		text, entry := quotaRow(id, &profile, snapshot, now)
		rows = append(rows, row{text, entry})
	}
	if only != nil && len(rows) == 0 {
		if withDisabled {
			return "", fmt.Errorf("no profile %s", core.RustDebugString(*only))
		}
		return "", fmt.Errorf("no enabled profile %s", core.RustDebugString(*only))
	}
	if json {
		profiles := make([]any, 0, len(rows))
		for _, row := range rows {
			profiles = append(profiles, row.entry)
		}
		encoded, err := core.SerdeJSONPretty(jsonvalue.Object{{Key: "cycles", Value: snapshot.Cycles}, {Key: "profiles", Value: profiles}})
		if err != nil {
			return "", fmt.Errorf("serialize quota reports: %w", err)
		}
		return string(encoded), nil
	}
	if len(rows) == 0 {
		return "no enabled profiles", nil
	}
	texts := make([]string, 0, len(rows))
	for _, row := range rows {
		texts = append(texts, row.text)
	}
	return strings.Join(texts, "\n"), nil
}

// hostsArgs are the arguments of `mj hosts`.
type hostsArgs struct {
	// host shows only this host or fleet, named by its host or by a target
	// it contains; default lists every host.
	host *string
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the hosts options on cmd.
func (args *hostsArgs) bind(cmd *cobra.Command, run func() error) {
	meta := clapOf(cmd)
	meta.option("host", "Show only this host or fleet, named by its host or by a target it contains; default lists every host", storeOptional(&args.host))
	meta.flag(&args.json, "json", jsonHelp)
	leaf(cmd, run)
}

// hosts reads the daemon capacity service's latest readings, shared with the
// terminal dashboard and the web viewer.
func hosts(ctx context.Context, args hostsArgs) error {
	daemon, err := ConnectOrStart(ctx)
	if err != nil {
		return err
	}
	defer daemon.Close()
	rows, err := daemon.HostCapacity(ctx)
	if err != nil {
		return err
	}
	rendered, err := renderHosts(rows, args.host, args.json, core.EpochSeconds())
	if err != nil {
		return err
	}
	fmt.Println(rendered)
	return nil
}

// renderHosts is what `mj hosts` prints at now.
func renderHosts(rows []core.HostCapacityRow, only *string, json bool, now uint64) (string, error) {
	selected := slices.Clone(rows)
	if only != nil {
		selected = nil
		known := map[string]bool{}
		for index := range rows {
			if rows[index].Matches(*only) {
				selected = append(selected, rows[index])
			}
			known[rows[index].Host] = true
			for _, id := range rows[index].TargetIDs {
				known[id] = true
			}
		}
		if len(selected) == 0 {
			if len(known) == 0 {
				return "", fmt.Errorf("no host or target %s: the daemon has no hosts", core.RustDebugString(*only))
			}
			return "", fmt.Errorf("no host or target %s; known names: %s", core.RustDebugString(*only), strings.Join(sortedKeys(known), ", "))
		}
	}
	if selected == nil {
		selected = []core.HostCapacityRow{}
	}
	if json {
		encoded, err := core.SerdeJSONPretty(jsonvalue.Object{{Key: "now_epoch_seconds", Value: now}, {Key: "hosts", Value: selected}})
		if err != nil {
			return "", fmt.Errorf("serialize host capacity: %w", err)
		}
		return string(encoded), nil
	}
	return core.RenderHostCapacity(selected, now), nil
}

// setConfig applies a session configuration setting, or lists the settings
// the session's agent offers.
func setConfig(ctx context.Context, args setConfigArgs) error {
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	var session server.ApiSession
	switch {
	case args.key != nil && args.value != nil:
		if session, err = api.setConfig(ctx, args.session, &server.SetConfigRequest{Key: *args.key, Value: *args.value}); err != nil {
			return err
		}
	// The settings are the agent's, so they differ by harness and model;
	// listing them is how a user learns which keys this command takes.
	case args.key == nil && args.value == nil:
		known, err := api.sessionIfKnown(ctx, args.session)
		if err != nil {
			return err
		}
		if known == nil {
			return fmt.Errorf("no session %s", args.session)
		}
		session = *known
	case args.key != nil:
		return errors.New("pass --value with --key")
	default:
		return errors.New("pass --key with --value")
	}
	if args.json {
		return printJSON(session)
	}
	if len(session.ConfigOptions) == 0 {
		fmt.Println("this session's agent offers no settings right now")
	}
	printLines(configOptionLines(session.ConfigOptions))
	return nil
}

// configOptionLines is one line per setting: its key, its current value, and
// what it accepts.
func configOptionLines(options []server.ViewerConfigOption) []string {
	lines := make([]string, 0, len(options))
	for _, option := range options {
		current := "unknown"
		if option.Current != nil {
			current = *option.Current
		}
		choices := make([]string, 0, len(option.Choices))
		for _, choice := range option.Choices {
			choices = append(choices, choice.Value)
		}
		lines = append(lines, fmt.Sprintf("%s  %s  (choices: %s)", option.Key, current, strings.Join(choices, ", ")))
	}
	return lines
}
