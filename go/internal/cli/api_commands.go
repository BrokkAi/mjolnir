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
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/checkpoint"
	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
	"github.com/BrokkAi/mjolnir/go/internal/server"
	"github.com/BrokkAi/mjolnir/go/internal/sessionruntime"
	"github.com/spf13/cobra"
)

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
	panic("unported: mj-cli/src/api_commands.rs EventsArgs (clap Args)")
}

// events follows durable session events as line-delimited JSON, following
// the daemon across an upgrade handoff from the cursor its stream reached.
func events(ctx context.Context, args eventsArgs, requestedWorkspace *string) error {
	panic("unported: mj-cli/src/api_commands.rs events")
}

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
	panic("unported: mj-cli/src/api_commands.rs NewArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs PromptArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs EventArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs MessageArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs message")
}

// event queues an external event in a session's durable mailbox.
func event(ctx context.Context, args eventArgs) error {
	panic("unported: mj-cli/src/api_commands.rs event")
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
	panic("unported: mj-cli/src/api_commands.rs WaitArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs TranscriptArgs (clap Args)")
}

// parseTranscriptRole is the value parser of `--role`.
func parseTranscriptRole(value string) (core.TranscriptRole, error) {
	panic("unported: mj-cli/src/api_commands.rs parse_transcript_role")
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
	panic("unported: mj-cli/src/api_commands.rs UsageArgs (clap Args)")
}

// usage reads recorded token usage and coverage.
func usage(ctx context.Context, args usageArgs) error {
	panic("unported: mj-cli/src/api_commands.rs usage")
}

// modelUsageLines is the per-model and effort totals, one counter per line.
func modelUsageLines(groups []core.UsageModelTotal) []string {
	panic("unported: mj-cli/src/api_commands.rs model_usage_lines")
}

// turnCostUSD is a priced turn cost in USD ticks as text.
func turnCostUSD(ticks uint64) string {
	panic("unported: mj-cli/src/api_commands.rs turn_cost_usd")
}

// usageTreeLines is what `mj usage --parent` prints without `--json`.
func usageTreeLines(tree *core.UsageTree) []string {
	panic("unported: mj-cli/src/api_commands.rs usage_tree_lines")
}

// usageLines is what `mj usage` prints without `--json`.
//
// The coverage is printed with the totals, never apart from them: a harness
// that reports only its last request, or nothing, would otherwise make a
// partial count read as the whole session's.
func usageLines(page *core.UsagePage) []string {
	panic("unported: mj-cli/src/api_commands.rs usage_lines")
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
	panic("unported: mj-cli/src/api_commands.rs PutFileArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs ElicitationsArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs RespondArgs (clap Args)")
}

// putFile atomically uploads a file, or standard input, to an idle session
// workspace.
func putFile(ctx context.Context, args putFileArgs) error {
	panic("unported: mj-cli/src/api_commands.rs put_file")
}

// elicitations prints a session's structured input requests as JSON.
func elicitations(ctx context.Context, args elicitationsArgs) error {
	panic("unported: mj-cli/src/api_commands.rs elicitations")
}

// respond answers a structured input request.
func respond(ctx context.Context, args respondArgs) error {
	panic("unported: mj-cli/src/api_commands.rs respond")
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
	panic("unported: mj-cli/src/api_commands.rs DiffArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs FetchArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs ExportArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs SessionsArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs SessionArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs StopTaskArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs stop_task")
}

// stopTasks asks the daemon to stop each selected task, together, and
// reports each outcome.
func stopTasks(ctx context.Context, api *apiClient, args *stopTaskArgs) (stopTaskReport, error) {
	panic("unported: mj-cli/src/api_commands.rs stop_tasks")
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
	panic("unported: mj-cli/src/api_commands.rs SuspendArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs DestroyArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs NoticesArgs (clap Args)")
}

// notices reads recent cleanup notices, including after a session was
// destroyed.
func notices(ctx context.Context, args noticesArgs) error {
	panic("unported: mj-cli/src/api_commands.rs notices")
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
	panic("unported: mj-cli/src/api_commands.rs ResumeArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs ApiInfoArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs GithubTokenArgs (clap Args)")
}

// workspacesListArgs are the arguments of `mj workspaces list`.
type workspacesListArgs struct {
	// json prints the response as JSON instead of text.
	json bool
}

// bind declares the workspaces list options on cmd.
func (args *workspacesListArgs) bind(cmd *cobra.Command, run func() error) {
	panic("unported: mj-cli/src/api_commands.rs WorkspacesListArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs WorkspaceCreateArgs (clap Args)")
}

// workspacesList lists the workspaces sessions can be created in.
func workspacesList(ctx context.Context, args workspacesListArgs) error {
	panic("unported: mj-cli/src/api_commands.rs workspaces_list")
}

// workspacesCreate creates a workspace, or selects the one that already
// carries the name.
func workspacesCreate(ctx context.Context, args workspaceCreateArgs) error {
	panic("unported: mj-cli/src/api_commands.rs workspaces_create")
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
	panic("unported: mj-cli/src/api_commands.rs subagent_policy")
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
	panic("unported: mj-cli/src/api_commands.rs session_review")
}

// newSession creates a session and delivers its optional first prompt.
func newSession(ctx context.Context, args newArgs, requestedWorkspace *string) error {
	panic("unported: mj-cli/src/api_commands.rs new_session")
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
	panic("unported: mj-cli/src/api_commands.rs resolve_new_session_project_target")
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
	panic("unported: mj-cli/src/api_commands.rs infer_new_session_inputs")
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
	panic("unported: mj-cli/src/api_commands.rs ClearQueueArgs (clap Args)")
}

// clearQueue cancels queued prompts while preserving the session and its
// current turn.
func clearQueue(ctx context.Context, args clearQueueArgs) error {
	panic("unported: mj-cli/src/api_commands.rs clear_queue")
}

// prompt sends a prompt as the user and waits for the turn it became.
func prompt(ctx context.Context, args promptArgs) error {
	panic("unported: mj-cli/src/api_commands.rs prompt")
}

// promptFrom is prompt with the calling session given. A prompt sent this
// way reads exactly as the user's own, so it refuses to run inside a
// session.
func promptFrom(ctx context.Context, args promptArgs, callingSession *string) error {
	panic("unported: mj-cli/src/api_commands.rs prompt_from")
}

// wait blocks until a turn ends and prints how it ended.
func wait(ctx context.Context, args waitArgs) error {
	panic("unported: mj-cli/src/api_commands.rs wait")
}

// followHandoff connects to the daemon that replaced one being upgraded.
// When that daemon is a newer protocol or release, which this client cannot
// speak to, it finishes the command under the daemon's own build, as
// dashboards do. continuation names only what is left of the command, so
// nothing is repeated.
func followHandoff(ctx context.Context, continuation []string) (*apiClient, error) {
	panic("unported: mj-cli/src/api_commands.rs follow_handoff")
}

// waitContinuation is the rest of a wait: the same session and turn, the
// time left, and the same output form.
func waitContinuation(session string, request *sessionruntime.WaitRequest, json bool) []string {
	panic("unported: mj-cli/src/api_commands.rs wait_continuation")
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
	panic("unported: mj-cli/src/api_commands.rs wait_following_handoffs")
}

// reportWait prints a wait result, and fails the process when the turn did
// not finish so a shell script can branch on it.
func reportWait(response *server.WaitResponse, json bool) error {
	panic("unported: mj-cli/src/api_commands.rs report_wait")
}

// suspendedCleanly is whether a wait ended because the session was
// suspended as asked, rather than because a resume or a close failed.
// `mj suspend` tells the user to watch with `mj wait`, so this ending is the
// success it was waiting for.
func suspendedCleanly(response *server.WaitResponse) bool {
	panic("unported: mj-cli/src/api_commands.rs suspended_cleanly")
}

// waitReportLines is what `mj wait` prints, one line per entry.
//
// Separate from the printing so the content can be tested: a turn that
// failed used to print the bare word "error" and leave the reason in the
// transcript, where automation never saw it (#1020).
func waitReportLines(response *server.WaitResponse) []string {
	panic("unported: mj-cli/src/api_commands.rs wait_report_lines")
}

// sessionReportLines is what `mj sessions --session` prints for one session,
// one line per entry.
func sessionReportLines(session *server.ApiSession, nowMS int64) []string {
	panic("unported: mj-cli/src/api_commands.rs session_report_lines")
}

// operationStartTime is an operation's start as RFC 3339 in UTC, to the
// second.
func operationStartTime(seconds uint64) string {
	panic("unported: mj-cli/src/api_commands.rs operation_start_time")
}

// relayStateName is the relay state in the words `mj wait` and
// `mj sessions` use.
func relayStateName(state server.RelayState) string {
	panic("unported: mj-cli/src/api_commands.rs relay_state_name")
}

// outcomeName is the wait outcome's wire name.
func outcomeName(outcome sessionruntime.WaitOutcome) string {
	panic("unported: mj-cli/src/api_commands.rs outcome_name")
}

// transcript pages through a session's transcript.
func transcript(ctx context.Context, args transcriptArgs) error {
	panic("unported: mj-cli/src/api_commands.rs transcript")
}

// diff prints a unified diff of a session's work, and warns when HEAD does
// not descend from the base.
func diff(ctx context.Context, args diffArgs) error {
	panic("unported: mj-cli/src/api_commands.rs diff")
}

// export gets a session's work out as a patch, a pushed branch, or a git
// bundle.
func export(ctx context.Context, args exportArgs) error {
	panic("unported: mj-cli/src/api_commands.rs export")
}

// fetch copies a file, or a directory as a zip archive, from a session's
// target into this machine's temporary directory and prints where it landed.
func fetch(ctx context.Context, args fetchArgs) error {
	panic("unported: mj-cli/src/api_commands.rs fetch")
}

// listingLines is one line per entry, as `ls -l` puts it: the size
// right-aligned, the modification time in UTC, then the name, with `/` after
// a directory and the target after a symlink. A directory's own size says
// nothing about what it holds and differs between filesystems, so it shows
// as `-`.
func listingLines(listing *checkpoint.SessionListing) []string {
	panic("unported: mj-cli/src/api_commands.rs listing_lines")
}

// sessions lists the sessions the daemon holds, or reports one session.
func sessions(ctx context.Context, args sessionsArgs, requestedWorkspace *string) error {
	panic("unported: mj-cli/src/api_commands.rs sessions")
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
	panic("unported: mj-cli/src/api_commands.rs select_sessions")
}

// listedState is the STATE column: the session's state, and "disk full"
// when its target cannot take writes. `--session` prints the whole reason.
func listedState(session *server.ApiSession) string {
	panic("unported: mj-cli/src/api_commands.rs listed_state")
}

// wikiSession reports one SessionWiki row for an id that names no Mjolnir
// session.
//
// `mine` is a Mjolnir session the daemon still has a record of, `archived`
// one whose record the archive job destroyed, and `native` another tool's
// session. All three can be continued with `mj resume --wiki <id>`.
func wikiSession(ctx context.Context, api *apiClient, wikiID string, json bool) error {
	panic("unported: mj-cli/src/api_commands.rs wiki_session")
}

// wikiSessionLines is what `mj sessions --session` prints for a SessionWiki
// row.
func wikiSessionLines(info *client.WikiSessionInfo) []string {
	panic("unported: mj-cli/src/api_commands.rs wiki_session_lines")
}

// suspend saves a recovery copy and releases the environment. Acceptance is
// not completion.
func suspend(ctx context.Context, args suspendArgs) error {
	panic("unported: mj-cli/src/api_commands.rs suspend")
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
	panic("unported: mj-cli/src/api_commands.rs PurgeUntrackedArgs (clap Args)")
}

// purgeUntracked removes legacy untracked checkpoint payloads from stopped
// sessions.
func purgeUntracked(ctx context.Context, args purgeUntrackedArgs) error {
	panic("unported: mj-cli/src/api_commands.rs purge_untracked")
}

// purgeUntrackedConclusion is the text or JSON of a purge report, and
// whether the process should fail.
//
// A batch that rewrote some sessions and failed others still produces the
// whole report. The error comes after that report so a script sees every row
// and a non-zero status.
func purgeUntrackedConclusion(response *server.PurgeUntrackedResponse, json bool) (report string, status error) {
	panic("unported: mj-cli/src/api_commands.rs purge_untracked_conclusion")
}

// refuseFailedPurge fails when any session failed to purge.
func refuseFailedPurge(response *server.PurgeUntrackedResponse) error {
	panic("unported: mj-cli/src/api_commands.rs refuse_failed_purge")
}

// purgeUntrackedLines is one line per session, then the total. A dry run
// says what it would reclaim.
func purgeUntrackedLines(response *server.PurgeUntrackedResponse) []string {
	panic("unported: mj-cli/src/api_commands.rs purge_untracked_lines")
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
	panic("unported: mj-cli/src/api_commands.rs format_suspend_dry_run")
}

// destroy permanently destroys the session, environment, and recovery
// archive.
func destroy(ctx context.Context, args destroyArgs) error {
	panic("unported: mj-cli/src/api_commands.rs destroy")
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

// resume resumes a suspended session from its checkpoint.
//
// The API answers as soon as the daemon admits the resume, because restoring
// an archive onto a fresh target takes minutes. Follow it with
// `mj wait --session <id>`, which blocks while the resume runs and reports
// why it failed if it does.
func resume(ctx context.Context, args resumeArgs) error {
	panic("unported: mj-cli/src/api_commands.rs resume")
}

// resumeWithWarnings resumes a session and prints the launch notices the
// daemon produced meanwhile.
func resumeWithWarnings(ctx context.Context, api *apiClient, sessionID string, request *server.ResumeSessionRequest) (server.ResumeSessionResponse, error) {
	panic("unported: mj-cli/src/api_commands.rs resume_with_warnings")
}

// resumeWiki continues the session a SessionWiki id names, whoever ran it.
//
// The branch is the daemon's: a Mjolnir session Mjolnir still has a record
// of is resumed, one it has archived is restored from the indexed
// transcript, and another tool's session is imported and then resumed. The
// output says which of the three ran, so the caller can follow with
// `mj prompt` or `mj message`.
func resumeWiki(ctx context.Context, args resumeArgs, wikiID string) error {
	panic("unported: mj-cli/src/api_commands.rs resume_wiki")
}

// reportWikiContinuation is one line saying which of the three cases ran, or
// the same three facts as JSON. The `mj wait` hint is the same one a plain
// resume prints.
func reportWikiContinuation(args *resumeArgs, action, wikiID, sessionID string, summary func() string) error {
	panic("unported: mj-cli/src/api_commands.rs report_wiki_continuation")
}

// interruptTurn cancels the turn a session is running.
func interruptTurn(ctx context.Context, args sessionArgs) error {
	panic("unported: mj-cli/src/api_commands.rs interrupt_turn")
}

// restartSession restarts a running session's agent.
func restartSession(ctx context.Context, args sessionArgs) error {
	panic("unported: mj-cli/src/api_commands.rs restart_session")
}

// reviewArgs are the arguments of `mj review`.
type reviewArgs struct {
	command reviewCommand
}

// bind declares the review subcommands on cmd; run runs the parsed command.
func (args *reviewArgs) bind(cmd *cobra.Command, run func() error) {
	panic("unported: mj-cli/src/api_commands.rs ReviewArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs ReviewShowArgs (clap Args)")
}

// review reviews a session's committed work, or shows the review it has
// open.
func review(ctx context.Context, args reviewArgs) error {
	panic("unported: mj-cli/src/api_commands.rs review")
}

// reviewLines is a review as plain lines: its status, one line per role,
// then the verdict and its text.
func reviewLines(review *server.ViewerTurnReview) []string {
	panic("unported: mj-cli/src/api_commands.rs review_lines")
}

// finishedReviewLines is a finished review as plain lines, including each
// reviewer's full handback and the usage its deleted child session still
// accounts for.
func finishedReviewLines(response *server.FinishedReviewResponse) []string {
	panic("unported: mj-cli/src/api_commands.rs finished_review_lines")
}

// apiInfo prints the API location without exposing credentials.
func apiInfo(ctx context.Context, args apiInfoArgs) error {
	panic("unported: mj-cli/src/api_commands.rs api_info")
}

// apiToken is explicit credential output for callers authenticating outside
// the CLI.
func apiToken(ctx context.Context) error {
	panic("unported: mj-cli/src/api_commands.rs api_token")
}

// githubToken prints a valid GitHub App installation token for a selected
// owner or repo.
func githubToken(ctx context.Context, args githubTokenArgs) error {
	panic("unported: mj-cli/src/api_commands.rs github_token")
}

// readPrompt reads prompt text from an argument, a file, or standard input.
func readPrompt(text *string, file *string) (*string, error) {
	panic("unported: mj-cli/src/api_commands.rs read_prompt")
}

// readStdin reads the prompt from standard input, refusing a terminal.
func readStdin() (string, error) {
	panic("unported: mj-cli/src/api_commands.rs read_stdin")
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
	panic("unported: mj-cli/src/api_commands.rs write_bytes_to")
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
	panic("unported: mj-cli/src/api_commands.rs ModelsArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs SetConfigArgs (clap Args)")
}

// models discovers available models and efforts for a profile, or for every
// enabled profile.
func models(ctx context.Context, args modelsArgs) error {
	panic("unported: mj-cli/src/api_commands.rs models")
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
	panic("unported: mj-cli/src/api_commands.rs render_models_aggregate")
}

// renderModels is one profile's models and efforts, with prices when the
// profile bills by usage.
func renderModels(choices *core.ProfileConfig, billing client.QuotaBilling, json bool) (string, error) {
	panic("unported: mj-cli/src/api_commands.rs render_models")
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
	panic("unported: mj-cli/src/api_commands.rs QuotaArgs (clap Args)")
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
	panic("unported: mj-cli/src/api_commands.rs pump_quota_feed")
}

// quota shows each profile's remaining quota as the daemon last read it.
func quota(ctx context.Context, args quotaArgs) error {
	panic("unported: mj-cli/src/api_commands.rs quota")
}

// quotaRow is one profile's row for `mj quota`, or the reason its quota
// cannot be read: the text line and the JSON entry (an object whose keys
// serialize sorted, as serde_json's Value does).
func quotaRow(id string, profile *core.HarnessProfile, snapshot *client.QuotaSnapshot, now uint64) (string, map[string]any) {
	panic("unported: mj-cli/src/api_commands.rs quota_row")
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
	panic("unported: mj-cli/src/api_commands.rs render_quota_at")
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
	panic("unported: mj-cli/src/api_commands.rs HostsArgs (clap Args)")
}

// hosts reads the daemon capacity service's latest readings, shared with the
// terminal dashboard and the web viewer.
func hosts(ctx context.Context, args hostsArgs) error {
	panic("unported: mj-cli/src/api_commands.rs hosts")
}

// renderHosts is what `mj hosts` prints at now.
func renderHosts(rows []core.HostCapacityRow, only *string, json bool, now uint64) (string, error) {
	panic("unported: mj-cli/src/api_commands.rs render_hosts")
}

// setConfig applies a session configuration setting, or lists the settings
// the session's agent offers.
func setConfig(ctx context.Context, args setConfigArgs) error {
	panic("unported: mj-cli/src/api_commands.rs set_config")
}

// configOptionLines is one line per setting: its key, its current value, and
// what it accepts.
func configOptionLines(options []server.ViewerConfigOption) []string {
	panic("unported: mj-cli/src/api_commands.rs config_option_lines")
}
