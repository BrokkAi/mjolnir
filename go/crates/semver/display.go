// Port of semver 1.0.28 src/display.rs

package semver

import (
	"strconv"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// String is the `Display` output without padding: `1.2.3-pre+build`. It is
// what `to_string()` returns.
func (self *Version) String() string {
	text := strconv.FormatUint(self.Major, 10) + "." + strconv.FormatUint(self.Minor, 10) + "." + strconv.FormatUint(self.Patch, 10)
	if !self.Pre.IsEmpty() {
		text += "-" + self.Pre.String()
	}
	if !self.Build.IsEmpty() {
		text += "+" + self.Build.String()
	}
	return text
}

// FmtDisplay is `impl Display for Version`. A width pads the version with the
// fill character, on the right unless an alignment says otherwise. A
// precision is ignored.
func (self *Version) FmtDisplay(formatter *rs.Formatter) error {
	doDisplay := func(formatter *rs.Formatter) error {
		return formatter.WriteStr(self.String())
	}

	doLen := func() int {
		return displayDigits(self.Major) +
			1 +
			displayDigits(self.Minor) +
			1 +
			displayDigits(self.Patch) +
			displayBoolAsUsize(!self.Pre.IsEmpty()) +
			len(self.Pre.Deref()) +
			displayBoolAsUsize(!self.Build.IsEmpty()) +
			len(self.Build.Deref())
	}

	return displayPad(formatter, doDisplay, doLen)
}

// displayBoolAsUsize is `b as usize`.
func displayBoolAsUsize(b bool) int {
	if b {
		return 1
	}
	return 0
}

// String is the `Display` output: the identifier.
func (self *Prerelease) String() string {
	return self.AsStr()
}

// FmtDisplay is `impl Display for Prerelease`. The Rust impl writes with
// `write_str`, so width, fill and alignment of the caller's `{}` do not
// apply.
func (self *Prerelease) FmtDisplay(formatter *rs.Formatter) error {
	return formatter.WriteStr(self.AsStr())
}

// String is the `Display` output: the identifier.
func (self *BuildMetadata) String() string {
	return self.AsStr()
}

// FmtDisplay is `impl Display for BuildMetadata`. The Rust impl writes with
// `write_str`, so width, fill and alignment of the caller's `{}` do not
// apply.
func (self *BuildMetadata) FmtDisplay(formatter *rs.Formatter) error {
	return formatter.WriteStr(self.AsStr())
}

// FmtDebug is `impl Debug for Version`: a struct that leaves out the
// pre-release and the build metadata when they are empty.
func (self *Version) FmtDebug(formatter *rs.Formatter) error {
	debug := formatter.DebugStruct("Version")
	debug.
		Field("major", self.Major).
		Field("minor", self.Minor).
		Field("patch", self.Patch)
	if !self.Pre.IsEmpty() {
		debug.Field("pre", &self.Pre)
	}
	if !self.Build.IsEmpty() {
		debug.Field("build", &self.Build)
	}
	return debug.Finish()
}

// debugString is the `Debug` output: `Prerelease("alpha.1")`.
func (self *Prerelease) debugString() string {
	return "Prerelease(\"" + self.String() + "\")"
}

// FmtDebug is `impl Debug for Prerelease`.
func (self *Prerelease) FmtDebug(formatter *rs.Formatter) error {
	return formatter.WriteStr(self.debugString())
}

// debugString is the `Debug` output: `BuildMetadata("zstd.1.5.0")`.
func (self *BuildMetadata) debugString() string {
	return "BuildMetadata(\"" + self.String() + "\")"
}

// FmtDebug is `impl Debug for BuildMetadata`.
func (self *BuildMetadata) FmtDebug(formatter *rs.Formatter) error {
	return formatter.WriteStr(self.debugString())
}

func displayPad(
	formatter *rs.Formatter,
	doDisplay func(*rs.Formatter) error,
	doLen func() int,
) error {
	minWidth, ok := formatter.Width().Get()
	if !ok {
		return doDisplay(formatter)
	}

	len_ := doLen()
	if len_ >= minWidth {
		return doDisplay(formatter)
	}

	defaultAlign := rs.Alignment_Left
	align := formatter.Align().UnwrapOr(defaultAlign)
	padding := minWidth - len_
	var prePad, postPad int
	switch align {
	case rs.Alignment_Left:
		prePad, postPad = 0, padding
	case rs.Alignment_Right:
		prePad, postPad = padding, 0
	case rs.Alignment_Center:
		prePad, postPad = padding/2, (padding+1)/2
	default:
		panic(rs.Unreachable(align))
	}

	fill := formatter.Fill()
	for range prePad {
		if err := formatter.WriteChar(fill); err != nil {
			return err
		}
	}

	if err := doDisplay(formatter); err != nil {
		return err
	}

	for range postPad {
		if err := formatter.WriteChar(fill); err != nil {
			return err
		}
	}
	return nil
}

func displayDigits(val uint64) int {
	if val < 10 {
		return 1
	} else {
		return 1 + displayDigits(val/10)
	}
}
