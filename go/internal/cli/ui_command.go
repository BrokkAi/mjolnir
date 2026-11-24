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
	"fmt"
	"strings"

	"github.com/BrokkAi/acp-go/jsonvalue"
	"github.com/spf13/cobra"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
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
	meta := clapOf(cmd)

	var pane uiCommandPane
	paneCmd := subcommand("pane", "Set one support pane's size, as its title-bar control does")
	meta.subcommand(paneCmd)
	paneClap := clapOf(paneCmd)
	paneClap.positional("pane", "The pane to resize", func(raw string) error {
		pane.pane = paneArg(raw)
		return nil
	}, possibleValues("sessions", "targets", "profiles"), required())
	paneClap.positional("size", "Its new size", func(raw string) error {
		pane.size = sizeArg(raw)
		return nil
	}, possibleValues("minimized", "standard", "maximized"), required())
	leaf(paneCmd, func() error {
		args.command = pane
		return run()
	})

	var show uiCommandShow
	showCmd := subcommand("show", "Show a session in the conversation area, or in a new pane beside it")
	meta.subcommand(showCmd)
	showClap := clapOf(showCmd)
	showClap.positional("session", "The session's full ID", storeString(&show.session), required())
	showClap.flag(&show.right, "right", "Open it in a new pane to the right of the conversation", conflictsWith("below"))
	showClap.flag(&show.below, "below", "Open it in a new pane below the conversation")
	leaf(showCmd, func() error {
		args.command = show
		return run()
	})

	var settings uiCommandSettings
	settingsCmd := subcommand("settings", "Open Settings with these changes staged for the person to review and save. Paths are the Settings page paths, which match the configuration file's keys: `notify.bell=false`, `theme=dark`, `advanced.auto_afk_minutes=30`. A value that reads as JSON (`true`, `30`, `null`) is used as that value; anything else is text, and `null` clears a field")
	meta.subcommand(settingsCmd)
	clapOf(settingsCmd).positional("edits", "One or more settings to stage, each as PATH=VALUE", appendString(&settings.edits),
		withValueName("PATH=VALUE"), required(), multiple())
	leaf(settingsCmd, func() error {
		args.command = settings
		return run()
	})

	meta.flag(&args.json, "json", "Print the daemon's answer as JSON instead of text", global())
	group(cmd, true, nil)
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
	var action client.UiAction
	switch command := args.command.(type) {
	case uiCommandPane:
		pane := map[paneArg]client.UiPane{
			paneArgSessions: client.UiPaneSessions,
			paneArgTargets:  client.UiPaneTargets,
			paneArgProfiles: client.UiPaneProfiles,
		}[command.pane]
		size := map[sizeArg]core.PaneSize{
			sizeArgMinimized: core.PaneSizeMinimized,
			sizeArgStandard:  core.PaneSizeStandard,
			sizeArgMaximized: core.PaneSizeMaximized,
		}[command.size]
		action = client.UiActionPaneSize{Pane: pane, Size: size}
	case uiCommandShow:
		placement := client.SessionPlacementMain
		if command.right {
			placement = client.SessionPlacementRight
		} else if command.below {
			placement = client.SessionPlacementBelow
		}
		action = client.UiActionShowSession{SessionID: command.session, Placement: placement}
	case uiCommandSettings:
		edits := make([]client.SettingEdit, 0, len(command.edits))
		for _, text := range command.edits {
			edit, err := settingEdit(text)
			if err != nil {
				return err
			}
			edits = append(edits, edit)
		}
		action = client.UiActionStageSettings{Edits: edits}
	}
	staged := -1
	if stage, ok := action.(client.UiActionStageSettings); ok {
		staged = len(stage.Edits)
	}
	from, err := callingSession()
	if err != nil {
		return err
	}
	api, err := apiClientConnect(ctx)
	if err != nil {
		return err
	}
	receipt, err := api.requestUI(ctx, &client.UiRequestBody{FromSessionID: from, Action: action})
	if err != nil {
		return err
	}
	if args.json {
		encoded, err := core.SerdeJSONPretty(receipt)
		if err != nil {
			return fmt.Errorf("serialize the API response: %w", err)
		}
		fmt.Println(string(encoded))
		return nil
	}
	dashboards := fmt.Sprintf("%d dashboards", receipt.Dashboards)
	if receipt.Dashboards == 1 {
		dashboards = "1 dashboard"
	}
	if staged < 0 {
		fmt.Printf("Sent to %s.\n", dashboards)
		return nil
	}
	plural := "s"
	if staged == 1 {
		plural = ""
	}
	fmt.Printf("Staged %d setting%s in Settings on %s. Nothing is saved until the person presses Save.\n", staged, plural, dashboards)
	return nil
}

// settingEdit is `notify.bell=false` as a staged edit.
func settingEdit(text string) (client.SettingEdit, error) {
	path, value, ok := strings.Cut(text, "=")
	if !ok {
		return client.SettingEdit{}, fmt.Errorf("%s is not PATH=VALUE, for example notify.bell=false", core.RustDebugString(text))
	}
	segments := strings.Split(strings.TrimSpace(path), ".")
	for _, segment := range segments {
		if segment == "" {
			return client.SettingEdit{}, fmt.Errorf("%s names an empty setting path", core.RustDebugString(text))
		}
	}
	// A value that reads as JSON is that value, as serde_json reads and
	// re-writes it; anything else is text.
	parsed, err := jsonvalue.Parse([]byte(value))
	if err != nil {
		parsed = value
	}
	encoded, err := jsonvalue.Marshal(parsed)
	if err != nil {
		return client.SettingEdit{}, err
	}
	return client.SettingEdit{Path: segments, Value: encoded}, nil
}
