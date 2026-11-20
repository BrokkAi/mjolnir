// Port of clap_builder 4.6.0 src/util/id.rs

package clap

import "github.com/BrokkAi/mjolnir/go/rs"

// Id is clap::Id: Arg or ArgGroup identifier.
//
// This is used for accessing the value in ArgMatches or defining
// relationships between Args and ArgGroups with functions like
// Arg::conflicts_with.
//
// It is a string: an Id compares with a string literal, and the builder
// methods that take `impl Into<Id>` take a string.
type Id string

const (
	idHelp     = "help"
	idVersion  = "version"
	idExternal = ""
)

// AsStr is Id::as_str: get the raw string of the Id.
func (id Id) AsStr() string { return string(id) }

// String is `impl Display for Id`.
func (id Id) String() string { return string(id) }

// FmtDisplay is `impl Display for Id`.
func (id Id) FmtDisplay(f *rs.Formatter) error { return f.Pad(string(id)) }

// FmtDebug is `impl Debug for Id`: the Debug form of the string.
func (id Id) FmtDebug(f *rs.Formatter) error { return f.WriteStr(strDebug(string(id))) }

func idsFromStrings(names []string) []Id {
	ids := make([]Id, len(names))
	for i, name := range names {
		ids[i] = Id(name)
	}
	return ids
}

func idsContain(ids []Id, id Id) bool {
	for _, existing := range ids {
		if existing == id {
			return true
		}
	}
	return false
}

func cloneIds(ids []Id) []Id {
	if ids == nil {
		return nil
	}
	return append([]Id(nil), ids...)
}
