// Port of clap_builder 4.6.0 src/builder/arg_settings.rs and
// src/builder/app_settings.rs

package clap

type argFlags uint32

func (f *argFlags) set(setting argSettings)       { *f |= setting.bit() }
func (f *argFlags) unset(setting argSettings)     { *f &^= setting.bit() }
func (f argFlags) isSet(setting argSettings) bool { return f&setting.bit() != 0 }

// argSettings is ArgSettings: various settings that apply to arguments and
// may be set, unset, and checked using getter/setter methods.
type argSettings uint8

const (
	argSettings_Required argSettings = iota
	argSettings_Global
	argSettings_Hidden
	argSettings_NextLineHelp
	argSettings_HidePossibleValues
	argSettings_AllowHyphenValues
	argSettings_AllowNegativeNumbers
	argSettings_RequireEquals
	argSettings_Last
	argSettings_TrailingVarArg
	argSettings_HideDefaultValue
	argSettings_IgnoreCase
	argSettings_HideEnv
	argSettings_HideEnvValues
	argSettings_HiddenShortHelp
	argSettings_HiddenLongHelp
	argSettings_Exclusive
)

func (s argSettings) bit() argFlags { return 1 << s }

type appFlags uint32

func (f *appFlags) set(setting appSettings)       { *f |= setting.bit() }
func (f *appFlags) unset(setting appSettings)     { *f &^= setting.bit() }
func (f appFlags) isSet(setting appSettings) bool { return f&setting.bit() != 0 }

// appSettings is AppSettings: application level settings, which affect how
// Command operates.
//
// NOTE: When these settings are used, they apply only to current command,
// and are not propagated down or up through child or parent subcommands.
type appSettings uint8

const (
	appSettings_IgnoreErrors appSettings = iota
	appSettings_AllowHyphenValues
	appSettings_AllowNegativeNumbers
	appSettings_AllArgsOverrideSelf
	appSettings_AllowMissingPositional
	appSettings_TrailingVarArg
	appSettings_DontDelimitTrailingValues
	appSettings_InferLongArgs
	appSettings_InferSubcommands
	appSettings_SubcommandRequired
	appSettings_AllowExternalSubcommands
	appSettings_Multicall
	appSettings_SubcommandsNegateReqs
	appSettings_ArgsNegateSubcommands
	appSettings_SubcommandPrecedenceOverArg
	appSettings_FlattenHelp
	appSettings_ArgRequiredElseHelp
	appSettings_NextLineHelp
	appSettings_DisableColoredHelp
	appSettings_DisableHelpFlag
	appSettings_DisableHelpSubcommand
	appSettings_DisableVersionFlag
	appSettings_PropagateVersion
	appSettings_Hidden
	appSettings_HidePossibleValues
	appSettings_HelpExpected
	appSettings_NoBinaryName
	appSettings_ColorAuto
	appSettings_ColorAlways
	appSettings_ColorNever
	appSettings_Built
	appSettings_BinNameBuilt
)

func (s appSettings) bit() appFlags { return 1 << s }
