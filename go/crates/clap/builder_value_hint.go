// Port of clap_builder 4.6.0 src/builder/value_hint.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// ValueHint is clap::ValueHint: provide shell with hint on how to complete
// an argument.
//
// See Arg::value_hint to set this on an argument.
//
// See the `clap_complete` crate for completion script generation.
type ValueHint uint8

const (
	// ValueHint_Unknown: default value if hint is not specified. Follows
	// shell default behavior, which is usually auto-completing filenames.
	ValueHint_Unknown ValueHint = iota
	// ValueHint_Other: none of the hints below apply. Disables shell
	// completion for this argument.
	ValueHint_Other
	// ValueHint_AnyPath: any existing path.
	ValueHint_AnyPath
	// ValueHint_FilePath: path to a file.
	ValueHint_FilePath
	// ValueHint_DirPath: path to a directory.
	ValueHint_DirPath
	// ValueHint_ExecutablePath: path to an executable file.
	ValueHint_ExecutablePath
	// ValueHint_CommandName: name of a command, without arguments. May be
	// relative to PATH, or full path to executable.
	ValueHint_CommandName
	// ValueHint_CommandString: a single string containing a command and its
	// arguments.
	ValueHint_CommandString
	// ValueHint_CommandWithArguments: capture the remaining arguments as a
	// command name and arguments for that command. This is common when
	// writing shell wrappers that execute anther command, for example
	// `sudo` or `env`.
	//
	// This hint is special, the argument must be a positional argument and
	// have .num_args(1..) and Arg::trailing_var_arg(true). Use
	// Command::trailing_var_arg(true) which stops parsing further arguments
	// once this one is parsed.
	ValueHint_CommandWithArguments
	// ValueHint_Username: name of a local operating system user.
	ValueHint_Username
	// ValueHint_Hostname: host name of a computer. Shells usually parse
	// `/etc/hosts` and `.ssh/known_hosts` to complete hostnames.
	ValueHint_Hostname
	// ValueHint_Url: complete web address.
	ValueHint_Url
	// ValueHint_EmailAddress: email address.
	ValueHint_EmailAddress
)

var _ = rs.RegisterEnum[ValueHint]("ValueHint",
	rs.Const(ValueHint_Unknown, "Unknown"),
	rs.Const(ValueHint_Other, "Other"),
	rs.Const(ValueHint_AnyPath, "AnyPath"),
	rs.Const(ValueHint_FilePath, "FilePath"),
	rs.Const(ValueHint_DirPath, "DirPath"),
	rs.Const(ValueHint_ExecutablePath, "ExecutablePath"),
	rs.Const(ValueHint_CommandName, "CommandName"),
	rs.Const(ValueHint_CommandString, "CommandString"),
	rs.Const(ValueHint_CommandWithArguments, "CommandWithArguments"),
	rs.Const(ValueHint_Username, "Username"),
	rs.Const(ValueHint_Hostname, "Hostname"),
	rs.Const(ValueHint_Url, "Url"),
	rs.Const(ValueHint_EmailAddress, "EmailAddress"),
)

// ValueHintDefault is `impl Default for ValueHint`.
func ValueHintDefault() ValueHint { return ValueHint_Unknown }

// ValueHintFromStr is `impl FromStr for ValueHint`.
func ValueHintFromStr(s string) (ValueHint, error) {
	switch strToAsciiLowercase(s) {
	case "unknown":
		return ValueHint_Unknown, nil
	case "other":
		return ValueHint_Other, nil
	case "anypath":
		return ValueHint_AnyPath, nil
	case "filepath":
		return ValueHint_FilePath, nil
	case "dirpath":
		return ValueHint_DirPath, nil
	case "executablepath":
		return ValueHint_ExecutablePath, nil
	case "commandname":
		return ValueHint_CommandName, nil
	case "commandstring":
		return ValueHint_CommandString, nil
	case "commandwitharguments":
		return ValueHint_CommandWithArguments, nil
	case "username":
		return ValueHint_Username, nil
	case "hostname":
		return ValueHint_Hostname, nil
	case "url":
		return ValueHint_Url, nil
	case "emailaddress":
		return ValueHint_EmailAddress, nil
	default:
		return 0, stringError("unknown ValueHint: `" + s + "`")
	}
}
