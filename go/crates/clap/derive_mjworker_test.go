// The command line of mj-worker (mj-worker-bin/src/main.rs, the clap
// items) in the Go spelling of a derive that doc.go describes, checked
// against the real derive run by the oracle (go/port/oracle/src/
// clap_oracle.rs, module mj_worker): parsing, errors and help.
//
// Port of mj-worker-bin/src/main.rs (the clap items only)

package clap

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type main_Cli struct {
	loginEnvironmentReady bool             `rs:"login_environment_ready"`
	workerRootLockFd      rs.Option[int32] `rs:"worker_root_lock_fd"`
	command               main_Command     `rs:"command"`
}

type main_Command interface{ ismain_Command() }

type main_Command_Worker struct{ F0 main_WorkerArgs }

func (*main_Command_Worker) ismain_Command() {}

type main_WorkerArgs struct {
	command main_WorkerCommand `rs:"command"`
}

type main_WorkerCommand interface{ ismain_WorkerCommand() }

type main_WorkerCommand_MoveWorkspace struct{}
type main_WorkerCommand_Liveness struct {
	Root string `rs:"root"`
}
type main_WorkerCommand_Holder struct {
	Root string `rs:"root"`
}
type main_WorkerCommand_Stop struct {
	Root string `rs:"root"`
	Kill bool   `rs:"kill"`
}
type main_WorkerCommand_BuildId struct{}
type main_WorkerCommand_DiscoverConfig struct {
	Spec string `rs:"spec"`
}
type main_WorkerCommand_Run struct {
	Root   string `rs:"root"`
	Config string `rs:"config"`
}
type main_WorkerCommand_PrepareHarness struct {
	Config string `rs:"config"`
}
type main_WorkerCommand_Proxy struct {
	Root string `rs:"root"`
}
type main_WorkerCommand_AcpSupervisor struct {
	Spec string `rs:"spec"`
}
type main_WorkerCommand_ExportCheckpoint struct {
	Spec string `rs:"spec"`
}
type main_WorkerCommand_CaptureCheckpoint struct{}
type main_WorkerCommand_PackCheckpoint struct{}
type main_WorkerCommand_RestoreCheckpoint struct {
	Spec            string            `rs:"spec"`
	MissingParent   bool              `rs:"missing_parent"`
	MissingCheckout bool              `rs:"missing_checkout"`
	RelaySeed       rs.Option[string] `rs:"relay_seed"`
}
type main_WorkerCommand_DiscardQueuedPrompts struct {
	Root      string `rs:"root"`
	SessionId string `rs:"session_id"`
	CommandId string `rs:"command_id"`
}
type main_WorkerCommand_InstallResource struct {
	Destination string `rs:"destination"`
}
type main_WorkerCommand_MemoryMcp struct {
	Root          rs.Option[string] `rs:"_root"`
	HistorySocket rs.Option[string] `rs:"history_socket"`
}
type main_WorkerCommand_MailboxHook struct {
	Socket string `rs:"socket"`
	Event  string `rs:"event"`
}
type main_WorkerCommand_MailboxRequest struct {
	Socket string `rs:"socket"`
}
type main_WorkerCommand_SubagentMcp struct {
	Socket                string                     `rs:"socket"`
	Harness               rs.Option[testHarnessKind] `rs:"harness"`
	Role                  testSubagentMcpRole        `rs:"role"`
	AgentMailboxesEnabled bool                       `rs:"agent_mailboxes_enabled"`
	SubtleSubagents       bool                       `rs:"subtle_subagents"`
	ReviewGate            bool                       `rs:"review_gate"`
	ReviewServer          bool                       `rs:"review_server"`
}
type main_WorkerCommand_Diff struct {
	Repository string            `rs:"repository"`
	Base       rs.Option[string] `rs:"base"`
	Branch     rs.Option[string] `rs:"branch"`
	Json       bool              `rs:"json"`
}
type main_WorkerCommand_ReadFile struct {
	Root string `rs:"root"`
	Path string `rs:"path"`
}
type main_WorkerCommand_Fetch struct {
	Path  string   `rs:"path"`
	Roots []string `rs:"roots"`
	Temp  bool     `rs:"temp"`
	List  bool     `rs:"list"`
}
type main_WorkerCommand_StoreFetch struct {
	Session   string `rs:"session"`
	Source    string `rs:"source"`
	Directory bool   `rs:"directory"`
	Length    int    `rs:"length"`
}
type main_WorkerCommand_WriteFile struct {
	Root      string `rs:"root"`
	Path      string `rs:"path"`
	Overwrite bool   `rs:"overwrite"`
	Length    int    `rs:"length"`
}
type main_WorkerCommand_PushBranch struct {
	Root       string `rs:"root"`
	Repository string `rs:"repository"`
	Branch     string `rs:"branch"`
}

func (*main_WorkerCommand_MoveWorkspace) ismain_WorkerCommand()        {}
func (*main_WorkerCommand_Liveness) ismain_WorkerCommand()             {}
func (*main_WorkerCommand_Holder) ismain_WorkerCommand()               {}
func (*main_WorkerCommand_Stop) ismain_WorkerCommand()                 {}
func (*main_WorkerCommand_BuildId) ismain_WorkerCommand()              {}
func (*main_WorkerCommand_DiscoverConfig) ismain_WorkerCommand()       {}
func (*main_WorkerCommand_Run) ismain_WorkerCommand()                  {}
func (*main_WorkerCommand_PrepareHarness) ismain_WorkerCommand()       {}
func (*main_WorkerCommand_Proxy) ismain_WorkerCommand()                {}
func (*main_WorkerCommand_AcpSupervisor) ismain_WorkerCommand()        {}
func (*main_WorkerCommand_ExportCheckpoint) ismain_WorkerCommand()     {}
func (*main_WorkerCommand_CaptureCheckpoint) ismain_WorkerCommand()    {}
func (*main_WorkerCommand_PackCheckpoint) ismain_WorkerCommand()       {}
func (*main_WorkerCommand_RestoreCheckpoint) ismain_WorkerCommand()    {}
func (*main_WorkerCommand_DiscardQueuedPrompts) ismain_WorkerCommand() {}
func (*main_WorkerCommand_InstallResource) ismain_WorkerCommand()      {}
func (*main_WorkerCommand_MemoryMcp) ismain_WorkerCommand()            {}
func (*main_WorkerCommand_MailboxHook) ismain_WorkerCommand()          {}
func (*main_WorkerCommand_MailboxRequest) ismain_WorkerCommand()       {}
func (*main_WorkerCommand_SubagentMcp) ismain_WorkerCommand()          {}
func (*main_WorkerCommand_Diff) ismain_WorkerCommand()                 {}
func (*main_WorkerCommand_ReadFile) ismain_WorkerCommand()             {}
func (*main_WorkerCommand_Fetch) ismain_WorkerCommand()                {}
func (*main_WorkerCommand_StoreFetch) ismain_WorkerCommand()           {}
func (*main_WorkerCommand_WriteFile) ismain_WorkerCommand()            {}
func (*main_WorkerCommand_PushBranch) ismain_WorkerCommand()           {}

func main_Cli_command() *Command {
	return main_Cli_augment_args(CommandNew("mj-worker"))
}

var main_Cli_Parser = Parser[main_Cli]{
	Command:           main_Cli_command,
	FromArgMatchesMut: main_Cli_from_arg_matches_mut,
}

func main_Cli_group_id() rs.Option[Id] { return rs.Some(Id("Cli")) }

func main_Cli_augment_args(cmd *Command) *Command {
	cmd = cmd.Group(ArgGroupNew("Cli").Multiple(true).Args("login_environment_ready", "worker_root_lock_fd"))
	cmd = cmd.Arg(ArgNew("login_environment_ready").
		ValueName("LOGIN_ENVIRONMENT_READY").
		Required(false).
		ValueParser(ValueParserBool()).
		Action(ArgAction_SetTrue).
		Help("Internal handoff: the parent explicitly supplied the clean login snapshot").
		Long("login-environment-ready").
		Hide(true).
		Global(true))
	cmd = cmd.Arg(ArgNew("worker_root_lock_fd").
		ValueName("WORKER_ROOT_LOCK_FD").
		ValueParser(ValueParserI32()).
		Action(ArgAction_Set).
		Help("Descriptor transferred by login-environment re-exec").
		Long("worker-root-lock-fd").
		Hide(true).
		Global(true))
	cmd = main_Command_augment_subcommands(cmd)
	cmd = cmd.SubcommandRequired(true).ArgRequiredElseHelp(true)
	return cmd.Version(mjCrateVersion).About("Mjolnir target-side worker")
}

func main_Cli_from_arg_matches_mut(m *ArgMatches) (main_Cli, error) {
	loginEnvironmentReadyValue, ok := ArgMatchesRemoveOne[bool](m, "login_environment_ready").Get()
	if !ok {
		return main_Cli{}, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: login_environment_ready")
	}
	workerRootLockFdValue := ArgMatchesRemoveOne[int32](m, "worker_root_lock_fd")
	commandValue, err := main_Command_from_arg_matches_mut(m)
	if err != nil {
		return main_Cli{}, err
	}
	return main_Cli{loginEnvironmentReady: loginEnvironmentReadyValue, workerRootLockFd: workerRootLockFdValue, command: commandValue}, nil
}

func main_Command_augment_subcommands(cmd *Command) *Command {
	{
		sub := CommandNew("worker")
		sub = main_WorkerArgs_augment_args(sub)
		sub = sub.About("Internal target-side worker commands").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	return cmd
}

func main_Command_has_subcommand(name string) bool {
	switch name {
	case "worker":
		return true
	}
	return false
}

func main_Command_from_arg_matches_mut(m *ArgMatches) (main_Command, error) {
	sc, ok := m.RemoveSubcommand().Get()
	if !ok {
		return nil, ErrorRaw(ErrorErrorKind_MissingSubcommand, "a subcommand is required but one was not provided")
	}
	scName, sub := sc.F0, sc.F1
	if scName == "worker" && !sub.ContainsId("") {
		value, err := main_WorkerArgs_from_arg_matches_mut(sub)
		if err != nil {
			return nil, err
		}
		return &main_Command_Worker{F0: value}, nil
	}
	return nil, ErrorRaw(ErrorErrorKind_InvalidSubcommand, "the subcommand '"+scName+"' wasn't recognized")
}

func main_WorkerArgs_group_id() rs.Option[Id] { return rs.Some(Id("WorkerArgs")) }

func main_WorkerArgs_augment_args(cmd *Command) *Command {
	cmd = cmd.Group(ArgGroupNew("WorkerArgs").Multiple(true).Args())
	cmd = main_WorkerCommand_augment_subcommands(cmd)
	cmd = cmd.SubcommandRequired(true).ArgRequiredElseHelp(true)
	return cmd
}

func main_WorkerArgs_from_arg_matches_mut(m *ArgMatches) (main_WorkerArgs, error) {
	commandValue, err := main_WorkerCommand_from_arg_matches_mut(m)
	if err != nil {
		return main_WorkerArgs{}, err
	}
	return main_WorkerArgs{command: commandValue}, nil
}

func main_WorkerCommand_augment_subcommands(cmd *Command) *Command {
	{
		sub := CommandNew("move-workspace")
		sub = sub.About("Move-only inventory and disk-backed workspace transfer utilities").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("liveness")
		sub = sub.Group(ArgGroupNew("Liveness").Multiple(true).Args("root"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.About("Print `alive`, `starting` or `dead` for the worker that owns a root, decided by the lock on its `worker.lock`").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("holder")
		sub = sub.Group(ArgGroupNew("Holder").Multiple(true).Args("root"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.About("Print the pid of the worker holding a root's lock, or nothing").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("stop")
		sub = sub.Group(ArgGroupNew("Stop").Multiple(true).Args("root", "kill"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.Arg(ArgNew("kill").
			ValueName("KILL").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Kill at once instead of allowing the TERM grace").
			Long("kill"))
		sub = sub.About("Stop the worker that owns a root and every process it started, and return only when its lock is free and none of them is left").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("build-id")
		sub = sub.About("Print this worker's compatibility identity. The controller asks an image's worker this before linking a session to it").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("discover-config")
		sub = sub.Group(ArgGroupNew("DiscoverConfig").Multiple(true).Args("spec"))
		sub = sub.Arg(ArgNew("spec").
			ValueName("SPEC").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("spec"))
		sub = sub.About("Discover profile model choices without submitting a prompt")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("run")
		sub = sub.Group(ArgGroupNew("Run").Multiple(true).Args("root", "config"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.Arg(ArgNew("config").
			ValueName("CONFIG").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("config"))
		sub = sub.About("Own an ACP bridge and durable session event log")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("prepare-harness")
		sub = sub.Group(ArgGroupNew("PrepareHarness").Multiple(true).Args("config"))
		sub = sub.Arg(ArgNew("config").
			ValueName("CONFIG").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("config"))
		sub = sub.About("Prepare an exact managed harness without starting a session worker")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("proxy")
		sub = sub.Group(ArgGroupNew("Proxy").Multiple(true).Args("root"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.About("Proxy JSON-lines between stdio and a detached worker")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("acp-supervisor")
		sub = sub.Group(ArgGroupNew("AcpSupervisor").Multiple(true).Args("spec"))
		sub = sub.Arg(ArgNew("spec").
			ValueName("SPEC").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("spec"))
		sub = sub.About("Supervise the ACP bridge process tree for a worker daemon")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("export-checkpoint")
		sub = sub.Group(ArgGroupNew("ExportCheckpoint").Multiple(true).Args("spec"))
		sub = sub.Arg(ArgNew("spec").
			ValueName("SPEC").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("Export specification path, or `-` to read it from standard input").
			Long("spec"))
		sub = sub.About("Build a target-side archive for verified controller transfer")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("capture-checkpoint")
		sub = sub.About("Seal target-owned checkpoint inputs while ACP dispatch is frozen").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("pack-checkpoint")
		sub = sub.About("Package a sealed checkpoint after ACP dispatch has resumed").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("restore-checkpoint")
		sub = sub.Group(ArgGroupNew("RestoreCheckpoint").Multiple(true).Args("spec", "missing_parent", "missing_checkout", "relay_seed"))
		sub = sub.Arg(ArgNew("spec").
			ValueName("SPEC").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("spec"))
		sub = sub.Arg(ArgNew("missing_parent").
			ValueName("MISSING_PARENT").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Restore only missing parent state; never modify checkout or child reports").
			Long("missing-parent").
			ConflictsWith("missing_checkout"))
		sub = sub.Arg(ArgNew("missing_checkout").
			ValueName("MISSING_CHECKOUT").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Long("missing-checkout"))
		sub = sub.Arg(ArgNew("relay_seed").
			ValueName("RELAY_SEED").
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("Current controller-owned queue/frontier, for partial parent recovery").
			Long("relay-seed").
			Requires("missing_parent"))
		sub = sub.About("Restore a verified archive into a freshly cloned target")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("discard-queued-prompts")
		sub = sub.Group(ArgGroupNew("DiscardQueuedPrompts").Multiple(true).Args("root", "session_id", "command_id"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.Arg(ArgNew("session_id").
			ValueName("SESSION_ID").
			Required(true).
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Long("session-id"))
		sub = sub.Arg(ArgNew("command_id").
			ValueName("COMMAND_ID").
			Required(true).
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Long("command-id"))
		sub = sub.About("Journal queue discard while the worker is stopped, before any dispatch").Hide(true)
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("install-resource")
		sub = sub.Group(ArgGroupNew("InstallResource").Multiple(true).Args("destination"))
		sub = sub.Arg(ArgNew("destination").
			ValueName("DESTINATION").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("destination"))
		sub = sub.About("Install one streamed resource directory on a remote target")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("memory-mcp")
		sub = sub.Group(ArgGroupNew("MemoryMcp").Multiple(true).Args("_root", "history_socket"))
		sub = sub.Arg(ArgNew("_root").
			ValueName("ROOT").
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("Accepted for staged Kimi profiles created by older controllers").
			Long("root").
			Hide(true))
		sub = sub.Arg(ArgNew("history_socket").
			ValueName("HISTORY_SOCKET").
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("history-socket"))
		sub = sub.About("Serve project history tools over MCP stdio")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("mailbox-hook")
		sub = sub.Group(ArgGroupNew("MailboxHook").Multiple(true).Args("socket", "event"))
		sub = sub.Arg(ArgNew("socket").
			ValueName("SOCKET").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("socket"))
		sub = sub.Arg(ArgNew("event").
			ValueName("EVENT").
			Required(true).
			ValueParser(BuilderPossibleValuesParserNew("PostToolUse", "PostToolBatch")).
			Action(ArgAction_Set).
			Long("event"))
		sub = sub.About("Drain mailbox events for one harness tool hook")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("mailbox-request")
		sub = sub.Group(ArgGroupNew("MailboxRequest").Multiple(true).Args("socket"))
		sub = sub.Arg(ArgNew("socket").
			ValueName("SOCKET").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("socket"))
		sub = sub.About("Exchange one mailbox lease request for a durable harness inbox")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("subagent-mcp")
		sub = sub.Group(ArgGroupNew("SubagentMcp").Multiple(true).Args("socket", "harness", "role", "agent_mailboxes_enabled", "subtle_subagents", "review_gate", "review_server"))
		sub = sub.Arg(ArgNew("socket").
			ValueName("SOCKET").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("socket"))
		sub = sub.Arg(ArgNew("harness").
			ValueName("HARNESS").
			ValueParser(ValueParserFromStr(testHarnessKindFromStr)).
			Action(ArgAction_Set).
			Help("Harness used to choose the MCP server instructions").
			Long("harness"))
		sub = sub.Arg(ArgNew("role").
			ValueName("ROLE").
			Required(false).
			ValueParser(ValueParserFromStr(testSubagentMcpRoleFromStr)).
			Action(ArgAction_Set).
			Help("`parent` serves the delegation tools; `child` serves only `handback`, a child's report to the session that started it").
			Long("role").
			DefaultValue("parent"))
		sub = sub.Arg(ArgNew("agent_mailboxes_enabled").
			ValueName("AGENT_MAILBOXES_ENABLED").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_Set).
			Help("Accepted for older launch arguments; tool availability is unchanged").
			Long("agent-mailboxes-enabled").
			DefaultValue("true"))
		sub = sub.Arg(ArgNew("subtle_subagents").
			ValueName("SUBTLE_SUBAGENTS").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_Set).
			Help("False when only the straightforward task type is configured: the spawn tool then offers no task type").
			Long("subtle-subagents").
			DefaultValue("true"))
		sub = sub.Arg(ArgNew("review_gate").
			ValueName("REVIEW_GATE").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Accepted for configurations written by older workers; the review tools now belong to the `--review-server` server").
			Long("review-gate").
			Hide(true))
		sub = sub.Arg(ArgNew("review_server").
			ValueName("REVIEW_SERVER").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Serve the before-push review tools as `mj-review` instead of the delegation tools").
			Long("review-server"))
		sub = sub.About("Serve Mjolnir-owned delegation tools over MCP stdio")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("diff")
		sub = sub.Group(ArgGroupNew("Diff").Multiple(true).Args("repository", "base", "branch", "json"))
		sub = sub.Arg(ArgNew("repository").
			ValueName("REPOSITORY").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("repository"))
		sub = sub.Arg(ArgNew("base").
			ValueName("BASE").
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Help("The commit the session started from, when the controller recorded one").
			Long("base"))
		sub = sub.Arg(ArgNew("branch").
			ValueName("BRANCH").
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Help("The session branch, whose reflog names the commit it was created at").
			Long("branch"))
		sub = sub.Arg(ArgNew("json").
			ValueName("JSON").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Include the resolved base and HEAD commit in JSON output").
			Long("json"))
		sub = sub.About("Print a unified diff of the session's work in one repository")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("read-file")
		sub = sub.Group(ArgGroupNew("ReadFile").Multiple(true).Args("root", "path"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.Arg(ArgNew("path").
			ValueName("PATH").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("Path relative to the workspace root").
			Long("path"))
		sub = sub.About("Write one file from the session workspace to standard output")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("fetch")
		sub = sub.Group(ArgGroupNew("Fetch").Multiple(true).Args("path", "roots", "temp", "list"))
		sub = sub.Arg(ArgNew("path").
			ValueName("PATH").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("The file or directory to fetch").
			Long("path"))
		sub = sub.Arg(ArgNew("roots").
			ValueName("ROOTS").
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Append).
			Help("A directory the path may resolve inside; repeatable").
			Long("root"))
		sub = sub.Arg(ArgNew("temp").
			ValueName("TEMP").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("Also allow this machine's temporary directory").
			Long("temp"))
		sub = sub.Arg(ArgNew("list").
			ValueName("LIST").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Help("List the path instead of reading it").
			Long("list"))
		sub = sub.About("Write a file, a zip archive of a directory, or a JSON listing of either to standard output, after one JSON line naming which it is and where it resolved")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("store-fetch")
		sub = sub.Group(ArgGroupNew("StoreFetch").Multiple(true).Args("session", "source", "directory", "length"))
		sub = sub.Arg(ArgNew("session").
			ValueName("SESSION").
			Required(true).
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Long("session"))
		sub = sub.Arg(ArgNew("source").
			ValueName("SOURCE").
			Required(true).
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Long("source"))
		sub = sub.Arg(ArgNew("directory").
			ValueName("DIRECTORY").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Long("directory"))
		sub = sub.Arg(ArgNew("length").
			ValueName("LENGTH").
			Required(true).
			ValueParser(ValueParserUsize()).
			Action(ArgAction_Set).
			Long("length"))
		sub = sub.About("Save fetched bytes in this target's temporary directory")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("write-file")
		sub = sub.Group(ArgGroupNew("WriteFile").Multiple(true).Args("root", "path", "overwrite", "length"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("root"))
		sub = sub.Arg(ArgNew("path").
			ValueName("PATH").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("path"))
		sub = sub.Arg(ArgNew("overwrite").
			ValueName("OVERWRITE").
			Required(false).
			ValueParser(ValueParserBool()).
			Action(ArgAction_SetTrue).
			Long("overwrite"))
		sub = sub.Arg(ArgNew("length").
			ValueName("LENGTH").
			Required(true).
			ValueParser(ValueParserUsize()).
			Action(ArgAction_Set).
			Help("Expected input length; premature EOF must not publish a partial file").
			Long("length"))
		sub = sub.About("Atomically publish stdin as a file in the session workspace")
		cmd = cmd.Subcommand(sub)
	}
	{
		sub := CommandNew("push-branch")
		sub = sub.Group(ArgGroupNew("PushBranch").Multiple(true).Args("root", "repository", "branch"))
		sub = sub.Arg(ArgNew("root").
			ValueName("ROOT").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Help("Worker root containing the session's existing Git authentication").
			Long("root"))
		sub = sub.Arg(ArgNew("repository").
			ValueName("REPOSITORY").
			Required(true).
			ValueParser(ValueParserPathBuf()).
			Action(ArgAction_Set).
			Long("repository"))
		sub = sub.Arg(ArgNew("branch").
			ValueName("BRANCH").
			Required(true).
			ValueParser(ValueParserString()).
			Action(ArgAction_Set).
			Long("branch"))
		sub = sub.About("Push the repository's current HEAD to a branch on its push remote")
		cmd = cmd.Subcommand(sub)
	}
	return cmd
}

func main_WorkerCommand_has_subcommand(name string) bool {
	switch name {
	case "move-workspace", "liveness", "holder", "stop", "build-id", "discover-config", "run", "prepare-harness", "proxy", "acp-supervisor", "export-checkpoint", "capture-checkpoint", "pack-checkpoint", "restore-checkpoint", "discard-queued-prompts", "install-resource", "memory-mcp", "mailbox-hook", "mailbox-request", "subagent-mcp", "diff", "read-file", "fetch", "store-fetch", "write-file", "push-branch":
		return true
	}
	return false
}

func main_WorkerCommand_from_arg_matches_mut(m *ArgMatches) (main_WorkerCommand, error) {
	sc, ok := m.RemoveSubcommand().Get()
	if !ok {
		return nil, ErrorRaw(ErrorErrorKind_MissingSubcommand, "a subcommand is required but one was not provided")
	}
	scName, sub := sc.F0, sc.F1
	if scName == "move-workspace" && !sub.ContainsId("") {
		return &main_WorkerCommand_MoveWorkspace{}, nil
	}
	if scName == "liveness" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		return &main_WorkerCommand_Liveness{Root: rootValue}, nil
	}
	if scName == "holder" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		return &main_WorkerCommand_Holder{Root: rootValue}, nil
	}
	if scName == "stop" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		killValue, ok := ArgMatchesRemoveOne[bool](sub, "kill").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: kill")
		}
		return &main_WorkerCommand_Stop{Root: rootValue, Kill: killValue}, nil
	}
	if scName == "build-id" && !sub.ContainsId("") {
		return &main_WorkerCommand_BuildId{}, nil
	}
	if scName == "discover-config" && !sub.ContainsId("") {
		specValue, ok := ArgMatchesRemoveOne[string](sub, "spec").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: spec")
		}
		return &main_WorkerCommand_DiscoverConfig{Spec: specValue}, nil
	}
	if scName == "run" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		configValue, ok := ArgMatchesRemoveOne[string](sub, "config").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: config")
		}
		return &main_WorkerCommand_Run{Root: rootValue, Config: configValue}, nil
	}
	if scName == "prepare-harness" && !sub.ContainsId("") {
		configValue, ok := ArgMatchesRemoveOne[string](sub, "config").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: config")
		}
		return &main_WorkerCommand_PrepareHarness{Config: configValue}, nil
	}
	if scName == "proxy" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		return &main_WorkerCommand_Proxy{Root: rootValue}, nil
	}
	if scName == "acp-supervisor" && !sub.ContainsId("") {
		specValue, ok := ArgMatchesRemoveOne[string](sub, "spec").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: spec")
		}
		return &main_WorkerCommand_AcpSupervisor{Spec: specValue}, nil
	}
	if scName == "export-checkpoint" && !sub.ContainsId("") {
		specValue, ok := ArgMatchesRemoveOne[string](sub, "spec").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: spec")
		}
		return &main_WorkerCommand_ExportCheckpoint{Spec: specValue}, nil
	}
	if scName == "capture-checkpoint" && !sub.ContainsId("") {
		return &main_WorkerCommand_CaptureCheckpoint{}, nil
	}
	if scName == "pack-checkpoint" && !sub.ContainsId("") {
		return &main_WorkerCommand_PackCheckpoint{}, nil
	}
	if scName == "restore-checkpoint" && !sub.ContainsId("") {
		specValue, ok := ArgMatchesRemoveOne[string](sub, "spec").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: spec")
		}
		missingParentValue, ok := ArgMatchesRemoveOne[bool](sub, "missing_parent").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: missing_parent")
		}
		missingCheckoutValue, ok := ArgMatchesRemoveOne[bool](sub, "missing_checkout").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: missing_checkout")
		}
		relaySeedValue := ArgMatchesRemoveOne[string](sub, "relay_seed")
		return &main_WorkerCommand_RestoreCheckpoint{Spec: specValue, MissingParent: missingParentValue, MissingCheckout: missingCheckoutValue, RelaySeed: relaySeedValue}, nil
	}
	if scName == "discard-queued-prompts" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		sessionIdValue, ok := ArgMatchesRemoveOne[string](sub, "session_id").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: session_id")
		}
		commandIdValue, ok := ArgMatchesRemoveOne[string](sub, "command_id").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: command_id")
		}
		return &main_WorkerCommand_DiscardQueuedPrompts{Root: rootValue, SessionId: sessionIdValue, CommandId: commandIdValue}, nil
	}
	if scName == "install-resource" && !sub.ContainsId("") {
		destinationValue, ok := ArgMatchesRemoveOne[string](sub, "destination").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: destination")
		}
		return &main_WorkerCommand_InstallResource{Destination: destinationValue}, nil
	}
	if scName == "memory-mcp" && !sub.ContainsId("") {
		rootValue := ArgMatchesRemoveOne[string](sub, "_root")
		historySocketValue := ArgMatchesRemoveOne[string](sub, "history_socket")
		return &main_WorkerCommand_MemoryMcp{Root: rootValue, HistorySocket: historySocketValue}, nil
	}
	if scName == "mailbox-hook" && !sub.ContainsId("") {
		socketValue, ok := ArgMatchesRemoveOne[string](sub, "socket").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: socket")
		}
		eventValue, ok := ArgMatchesRemoveOne[string](sub, "event").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: event")
		}
		return &main_WorkerCommand_MailboxHook{Socket: socketValue, Event: eventValue}, nil
	}
	if scName == "mailbox-request" && !sub.ContainsId("") {
		socketValue, ok := ArgMatchesRemoveOne[string](sub, "socket").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: socket")
		}
		return &main_WorkerCommand_MailboxRequest{Socket: socketValue}, nil
	}
	if scName == "subagent-mcp" && !sub.ContainsId("") {
		socketValue, ok := ArgMatchesRemoveOne[string](sub, "socket").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: socket")
		}
		harnessValue := ArgMatchesRemoveOne[testHarnessKind](sub, "harness")
		roleValue, ok := ArgMatchesRemoveOne[testSubagentMcpRole](sub, "role").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: role")
		}
		agentMailboxesEnabledValue, ok := ArgMatchesRemoveOne[bool](sub, "agent_mailboxes_enabled").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: agent_mailboxes_enabled")
		}
		subtleSubagentsValue, ok := ArgMatchesRemoveOne[bool](sub, "subtle_subagents").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: subtle_subagents")
		}
		reviewGateValue, ok := ArgMatchesRemoveOne[bool](sub, "review_gate").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: review_gate")
		}
		reviewServerValue, ok := ArgMatchesRemoveOne[bool](sub, "review_server").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: review_server")
		}
		return &main_WorkerCommand_SubagentMcp{Socket: socketValue, Harness: harnessValue, Role: roleValue, AgentMailboxesEnabled: agentMailboxesEnabledValue, SubtleSubagents: subtleSubagentsValue, ReviewGate: reviewGateValue, ReviewServer: reviewServerValue}, nil
	}
	if scName == "diff" && !sub.ContainsId("") {
		repositoryValue, ok := ArgMatchesRemoveOne[string](sub, "repository").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: repository")
		}
		baseValue := ArgMatchesRemoveOne[string](sub, "base")
		branchValue := ArgMatchesRemoveOne[string](sub, "branch")
		jsonValue, ok := ArgMatchesRemoveOne[bool](sub, "json").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: json")
		}
		return &main_WorkerCommand_Diff{Repository: repositoryValue, Base: baseValue, Branch: branchValue, Json: jsonValue}, nil
	}
	if scName == "read-file" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		pathValue, ok := ArgMatchesRemoveOne[string](sub, "path").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: path")
		}
		return &main_WorkerCommand_ReadFile{Root: rootValue, Path: pathValue}, nil
	}
	if scName == "fetch" && !sub.ContainsId("") {
		pathValue, ok := ArgMatchesRemoveOne[string](sub, "path").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: path")
		}
		rootsValue := ArgMatchesRemoveMany[string](sub, "roots").UnwrapOrDefault()
		if rootsValue == nil {
			rootsValue = []string{}
		}
		tempValue, ok := ArgMatchesRemoveOne[bool](sub, "temp").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: temp")
		}
		listValue, ok := ArgMatchesRemoveOne[bool](sub, "list").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: list")
		}
		return &main_WorkerCommand_Fetch{Path: pathValue, Roots: rootsValue, Temp: tempValue, List: listValue}, nil
	}
	if scName == "store-fetch" && !sub.ContainsId("") {
		sessionValue, ok := ArgMatchesRemoveOne[string](sub, "session").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: session")
		}
		sourceValue, ok := ArgMatchesRemoveOne[string](sub, "source").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: source")
		}
		directoryValue, ok := ArgMatchesRemoveOne[bool](sub, "directory").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: directory")
		}
		lengthValue, ok := ArgMatchesRemoveOne[int](sub, "length").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: length")
		}
		return &main_WorkerCommand_StoreFetch{Session: sessionValue, Source: sourceValue, Directory: directoryValue, Length: lengthValue}, nil
	}
	if scName == "write-file" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		pathValue, ok := ArgMatchesRemoveOne[string](sub, "path").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: path")
		}
		overwriteValue, ok := ArgMatchesRemoveOne[bool](sub, "overwrite").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: overwrite")
		}
		lengthValue, ok := ArgMatchesRemoveOne[int](sub, "length").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: length")
		}
		return &main_WorkerCommand_WriteFile{Root: rootValue, Path: pathValue, Overwrite: overwriteValue, Length: lengthValue}, nil
	}
	if scName == "push-branch" && !sub.ContainsId("") {
		rootValue, ok := ArgMatchesRemoveOne[string](sub, "root").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: root")
		}
		repositoryValue, ok := ArgMatchesRemoveOne[string](sub, "repository").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: repository")
		}
		branchValue, ok := ArgMatchesRemoveOne[string](sub, "branch").Get()
		if !ok {
			return nil, ErrorRaw(ErrorErrorKind_MissingRequiredArgument, "the following required argument was not provided: branch")
		}
		return &main_WorkerCommand_PushBranch{Root: rootValue, Repository: repositoryValue, Branch: branchValue}, nil
	}
	return nil, ErrorRaw(ErrorErrorKind_InvalidSubcommand, "the subcommand '"+scName+"' wasn't recognized")
}

func TestDeriveMjWorkerAgainstRust(t *testing.T) {
	checkDerived(t, "mj_worker", main_Cli_command, main_Cli_Parser, 40, [][]string{
		{"mj-worker", "worker", "run", "--root", "/r", "--config", "/c"},
		{"mj-worker", "--help"},
		{"mj-worker", "worker"},
		{"mj-worker"},
		{"mj-worker", "-V"},
		{"mj-worker", "worker", "subagent-mcp", "--socket", "/s", "--role", "bad"},
		{"mj-worker", "worker", "subagent-mcp", "--socket", "/s", "--harness", "claude", "--agent-mailboxes-enabled", "false"},
		{"mj-worker", "worker", "subagent-mcp", "--socket", "/s", "--agent-mailboxes-enabled"},
		{"mj-worker", "worker", "restore-checkpoint", "--spec", "/s", "--missing-parent", "--missing-checkout"},
		{"mj-worker", "worker", "restore-checkpoint", "--spec", "/s", "--relay-seed", "/q"},
		{"mj-worker", "worker", "fetch", "--path", "p", "--root", "/a", "--root", "/b", "--temp"},
		{"mj-worker", "worker", "memory-mcp", "--root", "/old"},
		{"mj-worker", "worker", "mailbox-hook", "--socket", "/s", "--event", "PostToolUse"},
		{"mj-worker", "worker", "mailbox-hook", "--socket", "/s", "--event", "posttooluse"},
		{"mj-worker", "worker", "store-fetch", "--session", "s", "--source", "x", "--length", "-1"},
		{"mj-worker", "--worker-root-lock-fd", "3", "worker", "liveness", "--root", "/r", "--login-environment-ready"},
		{"mj-worker", "worker", "liveness", "--root", "/r", "--worker-root-lock-fd", "x"},
		{"mj-worker", "worker", "help", "diff"},
		{"mj-worker", "worker", "diff", "--help"},
		{"mj-worker", "worker", "dif"},
		{"mj-worker", "worker", "write-file", "--root", "/r", "--path", "a", "--length", "10", "--overwrite"},
	})
}
