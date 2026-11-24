// Port of mj-cli/src/ui_command.rs.
//
// `mj ui`: change what the person's dashboard shows, on their behalf.
//
// Each attached dashboard applies the request as if the person had pressed
// the keys. `mj ui settings` opens Settings with the changes staged; they
// reach the configuration only when the person presses Save.

package cli

import (
	"context"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/spf13/cobra"
)

// uiArgs are the arguments of `mj ui`.
type uiArgs struct {
	command uiCommand
	// json prints the daemon's answer as JSON instead of text. Global to the
	// ui subcommands.
	json bool
}

// bind declares the ui subcommands and the global `--json` on cmd; run runs
// the parsed command.
func (args *uiArgs) bind(cmd *cobra.Command, run func() error) {
	panic("unported: mj-cli/src/ui_command.rs UiArgs (clap Args)")
}

// uiCommand is a `mj ui` subcommand.
type uiCommand interface{ isUiCommand() }

// uiCommandPane sets one support pane's size, as its title-bar control does.
type uiCommandPane struct {
	// pane is the pane to resize (positional).
	pane paneArg
	// size is its new size (positional).
	size sizeArg
}

// uiCommandShow shows a session in the conversation area, or in a new pane
// beside it.
type uiCommandShow struct {
	// session is the session's full ID (positional).
	session string
	// right opens it in a new pane to the right of the conversation.
	// Conflicts with below.
	right bool
	// below opens it in a new pane below the conversation.
	below bool
}

// uiCommandSettings opens Settings with these changes staged for the person
// to review and save. Paths are the Settings page paths, which match the
// configuration file's keys: `notify.bell=false`, `theme=dark`,
// `advanced.auto_afk_minutes=30`. A value that reads as JSON (`true`, `30`,
// `null`) is used as that value; anything else is text, and `null` clears a
// field.
type uiCommandSettings struct {
	// edits is one or more settings to stage, each as PATH=VALUE
	// (positional, required).
	edits []string
}

func (uiCommandPane) isUiCommand()     {}
func (uiCommandShow) isUiCommand()     {}
func (uiCommandSettings) isUiCommand() {}

// paneArg is the pane `mj ui pane` resizes.
type paneArg string

const (
	paneArgSessions paneArg = "sessions"
	paneArgTargets  paneArg = "targets"
	paneArgProfiles paneArg = "profiles"
)

// sizeArg is the size `mj ui pane` gives it.
type sizeArg string

const (
	sizeArgMinimized sizeArg = "minimized"
	sizeArgStandard  sizeArg = "standard"
	sizeArgMaximized sizeArg = "maximized"
)

// ui sends the request to every attached dashboard and says how many took
// it.
func ui(ctx context.Context, args uiArgs) error {
	panic("unported: mj-cli/src/ui_command.rs ui")
}

// settingEdit is `notify.bell=false` as a staged edit.
func settingEdit(text string) (client.SettingEdit, error) {
	panic("unported: mj-cli/src/ui_command.rs setting_edit")
}
