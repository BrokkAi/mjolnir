// Port of mj-client/src/build_identity.rs.

// Which of two Mjolnir builds is newer.
//
// The release version orders releases. Development builds, and two
// installations of one release, share a version string, a wire protocol and
// a schema, so the version alone cannot say which build a running daemon
// should give way to. Each build therefore also carries the Git revision it
// was built from, that commit's time, and when its executable file was
// written. The commit time orders different revisions; the file time orders
// two builds of one revision, which differ only in uncommitted changes or
// build settings.
//
// A daemon publishes these facts as semver build metadata on the
// `build_version` it already reports, for example
// `2.24.0+19485f17….c1790824791.m1790830000123456789`. `daemon.json` and the
// status reply reject unknown fields, so a new field would break every older
// client that reads them. Build metadata is invisible to older clients: they
// compare versions with `cmp_precedence`, which ignores it.

package client

import (
	"cmp"
	"errors"
	"fmt"
	"os"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode"
	"unicode/utf8"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// BuildIdentity is one build of Mjolnir, as far as it can be ordered against
// another.
type BuildIdentity struct {
	// version is the release version without build metadata, in semver
	// form (`2.24.0`, `2.24.0-rc.1`), as Rust's semver::Version displays it.
	version string
	// revision is the full Git revision. Nil for a daemon that predates
	// publishing it.
	revision *string
	// commitTime is the revision's committer time in Unix seconds, when the
	// build knew it.
	commitTime *int64
	// executableModified is when the executable file was last written, in
	// Unix nanoseconds (Rust holds a u128; a uint64 lasts until 2554).
	executableModified *uint64
}

const (
	commitTimePrefix         = "c"
	executableModifiedPrefix = "m"
)

// BuildIdentityParse reads a published `build_version`. A plain version, as
// every daemon before this one published, has no revision or times.
// Unrecognized build metadata is ignored so a later build can publish more.
func BuildIdentityParse(published string) (BuildIdentity, error) {
	version, metadata, err := parseSemver(published)
	if err != nil {
		return BuildIdentity{}, fmt.Errorf("parse build version %s: %w", buildIdentityDebug(published), err)
	}
	identity := BuildIdentity{version: version.String()}
	for _, part := range strings.Split(metadata, ".") {
		if part == "" {
			continue
		}
		if len(part) == 40 && isASCIIHex(part) {
			revision := strings.ToLower(part)
			identity.revision = &revision
			continue
		}
		if rest, ok := strings.CutPrefix(part, commitTimePrefix); ok {
			if seconds, err := parseRustInt64(rest); err == nil {
				identity.commitTime = &seconds
				continue
			}
		}
		if rest, ok := strings.CutPrefix(part, executableModifiedPrefix); ok {
			if nanos, err := parseRustUint64(rest); err == nil {
				identity.executableModified = &nanos
			}
		}
	}
	return identity, nil
}

// Published is the `build_version` a daemon running this build publishes.
func (b BuildIdentity) Published() string {
	var metadata []string
	if b.revision != nil {
		metadata = append(metadata, *b.revision)
	}
	if b.commitTime != nil {
		metadata = append(metadata, commitTimePrefix+strconv.FormatInt(*b.commitTime, 10))
	}
	if b.executableModified != nil {
		metadata = append(metadata, executableModifiedPrefix+strconv.FormatUint(*b.executableModified, 10))
	}
	if len(metadata) == 0 {
		return b.version
	}
	return b.version + "+" + strings.Join(metadata, ".")
}

// Describe is the build for a message a person reads, such as
// `2.24.0+19485f17, committed 2026-09-30 15:31:22 UTC, built 2026-09-30 16:02:09 UTC`.
func (b BuildIdentity) Describe() string {
	text := b.version
	if b.revision != nil {
		text += "+" + (*b.revision)[:8]
	}
	const layout = "2006-01-02 15:04:05 UTC"
	if b.commitTime != nil {
		if at, ok := buildIdentityTimestamp(*b.commitTime, 0); ok {
			text += ", committed " + at.Format(layout)
		}
	}
	if b.executableModified != nil {
		nanos := *b.executableModified
		if at, ok := buildIdentityTimestamp(int64(nanos/1_000_000_000), int64(nanos%1_000_000_000)); ok {
			text += ", built " + at.Format(layout)
		}
	}
	return text
}

// buildIdentityTimestamp is chrono's DateTime::from_timestamp: false outside
// the range chrono represents (years -262143 to 262142).
func buildIdentityTimestamp(seconds, nanos int64) (time.Time, bool) {
	const minSeconds = -8_334_601_228_800
	const maxSeconds = 8_210_266_876_799
	if seconds < minSeconds || seconds > maxSeconds {
		return time.Time{}, false
	}
	return time.Unix(seconds, nanos).UTC(), true
}

// SameSource is whether b and other were built from the same release
// version and Git revision, whatever their executable files' times. That is
// what identifies a daemon running an image's own copy of this build.
func (b BuildIdentity) SameSource(other BuildIdentity) bool {
	return semverComparePrecedence(b.version, other.version) == 0 &&
		b.revision != nil && other.revision != nil && *b.revision == *other.revision
}

// Version is the release version, without build metadata.
func (b BuildIdentity) Version() string { return b.version }

// thisBuild decides this process's identity once.
var thisBuild = sync.OnceValue(thisBuildIdentity)

// ThisBuild is the identity of the build this process runs.
//
// The file time is read once per process, from the running inode on Linux,
// so replacing the file on disk later does not change what this process says
// about itself. A daemon publishes exactly this value.
func ThisBuild() *BuildIdentity { return thisBuild() }

// thisBuildIdentity reads this build's version, revision, commit time and
// executable time (the body of Rust's OnceLock initializer).
func thisBuildIdentity() *BuildIdentity {
	version, _, err := parseSemver(core.PackageVersion)
	if err != nil {
		panic("the workspace version is semver")
	}
	identity := &BuildIdentity{version: version.String()}
	buildID := core.BuildID()
	if index := strings.LastIndex(buildID, "+"); index >= 0 {
		revision := strings.ToLower(buildID[index+1:])
		identity.revision = &revision
	}
	if seconds, err := parseRustInt64(core.CommitTime()); err == nil {
		identity.commitTime = &seconds
	}
	identity.executableModified = runningExecutableModified()
	return identity
}

func runningExecutableModified() *uint64 {
	path := "/proc/self/exe"
	if runtime.GOOS != "linux" {
		current, err := os.Executable()
		if err != nil {
			return nil
		}
		path = current
	}
	info, err := os.Stat(path)
	if err != nil {
		return nil
	}
	nanos := info.ModTime().UnixNano()
	if nanos < 0 {
		return nil
	}
	modified := uint64(nanos)
	return &modified
}

// DaemonBuildOrder is how a running daemon's build stands against a
// client's.
type DaemonBuildOrder int

const (
	// DaemonBuildOrderOlder: the client's build is newer: the client
	// replaces the daemon.
	DaemonBuildOrderOlder DaemonBuildOrder = iota
	// DaemonBuildOrderSame: the same build: the client uses the daemon.
	DaemonBuildOrderSame
	// DaemonBuildOrderNewer: the daemon's build is newer: the client must
	// never replace it.
	DaemonBuildOrderNewer
	// DaemonBuildOrderUnknown: the facts do not establish an order: the
	// client must not replace it.
	DaemonBuildOrderUnknown
)

// daemonBuildOrderFromDaemonOrdering maps the daemon's ordering against the
// client (negative, zero, positive, as cmp.Compare returns it).
func daemonBuildOrderFromDaemonOrdering(ordering int) DaemonBuildOrder {
	switch {
	case ordering < 0:
		return DaemonBuildOrderOlder
	case ordering == 0:
		return DaemonBuildOrderSame
	default:
		return DaemonBuildOrderNewer
	}
}

// CompareDaemonBuild orders daemon against client.
//
// Releases are ordered by version. Within one release, different revisions
// are ordered by commit time, and two builds of one revision by when their
// executables were written. A daemon that publishes no revision was built
// before any build published one, so it is the older build. Anything else
// that is missing, or a tie between different revisions, leaves the order
// unknown.
func CompareDaemonBuild(daemon, client *BuildIdentity) DaemonBuildOrder {
	if ordering := semverComparePrecedence(daemon.version, client.version); ordering != 0 {
		return daemonBuildOrderFromDaemonOrdering(ordering)
	}
	if client.revision == nil {
		return DaemonBuildOrderUnknown
	}
	if daemon.revision == nil {
		return DaemonBuildOrderOlder
	}
	if *daemon.revision == *client.revision {
		if daemon.executableModified != nil && client.executableModified != nil {
			return daemonBuildOrderFromDaemonOrdering(cmp.Compare(*daemon.executableModified, *client.executableModified))
		}
		return DaemonBuildOrderUnknown
	}
	if daemon.commitTime != nil && client.commitTime != nil && *daemon.commitTime != *client.commitTime {
		return daemonBuildOrderFromDaemonOrdering(cmp.Compare(*daemon.commitTime, *client.commitTime))
	}
	return DaemonBuildOrderUnknown
}

// semverVersion is a version as Rust's semver::Version holds it, without its
// build metadata.
type semverVersion struct {
	major, minor, patch uint64
	pre                 string
}

func (v semverVersion) String() string {
	text := fmt.Sprintf("%d.%d.%d", v.major, v.minor, v.patch)
	if v.pre != "" {
		text += "-" + v.pre
	}
	return text
}

// parseSemver reads a version as semver::Version::parse does, returning its
// build metadata separately.
func parseSemver(text string) (semverVersion, string, error) {
	var version semverVersion
	if text == "" {
		return version, "", errors.New("empty string, expected a semver version")
	}
	rest := text
	positions := [3]string{"major", "minor", "patch"}
	numbers := [3]*uint64{&version.major, &version.minor, &version.patch}
	for index, position := range positions {
		number, remaining, err := semverNumber(rest, position)
		if err != nil {
			return version, "", err
		}
		*numbers[index] = number
		rest = remaining
		if index == 2 {
			break
		}
		if !strings.HasPrefix(rest, ".") {
			if rest == "" {
				return version, "", fmt.Errorf("unexpected end of input while parsing %s version number", positions[index+1])
			}
			return version, "", fmt.Errorf("unexpected character %s after %s version number", semverCharDebug(rest), position)
		}
		rest = rest[1:]
	}
	if pre, ok := strings.CutPrefix(rest, "-"); ok {
		end := strings.IndexByte(pre, '+')
		if end < 0 {
			end = len(pre)
		}
		if err := semverIdentifiers(pre[:end], "pre-release", true); err != nil {
			return version, "", err
		}
		version.pre = pre[:end]
		rest = pre[end:]
	}
	var build string
	if metadata, ok := strings.CutPrefix(rest, "+"); ok {
		if err := semverIdentifiers(metadata, "build metadata", false); err != nil {
			return version, "", err
		}
		build = metadata
		rest = ""
	}
	if rest != "" {
		return version, "", fmt.Errorf("unexpected character %s after patch version number", semverCharDebug(rest))
	}
	return version, build, nil
}

// semverNumber reads one numeric version component.
func semverNumber(text, position string) (uint64, string, error) {
	end := 0
	for end < len(text) && text[end] >= '0' && text[end] <= '9' {
		end++
	}
	if end == 0 {
		if text == "" {
			return 0, text, fmt.Errorf("unexpected end of input while parsing %s version number", position)
		}
		return 0, text, fmt.Errorf("unexpected character %s while parsing %s version number", semverCharDebug(text), position)
	}
	if end > 1 && text[0] == '0' {
		return 0, text, fmt.Errorf("invalid leading zero in %s version number", position)
	}
	number, err := strconv.ParseUint(text[:end], 10, 64)
	if err != nil {
		return 0, text, fmt.Errorf("value of %s version number exceeds u64::MAX", position)
	}
	return number, text[end:], nil
}

// semverIdentifiers checks dot-separated pre-release or build identifiers.
func semverIdentifiers(text, position string, rejectLeadingZero bool) error {
	for _, identifier := range strings.Split(text, ".") {
		if identifier == "" {
			return fmt.Errorf("empty identifier segment in %s", position)
		}
		for i := 0; i < len(identifier); i++ {
			c := identifier[i]
			if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '-') {
				return fmt.Errorf("unexpected character %s while parsing %s", semverCharDebug(identifier[i:]), position)
			}
		}
		if rejectLeadingZero && isASCIIDigits(identifier) && len(identifier) > 1 && identifier[0] == '0' {
			return fmt.Errorf("invalid leading zero in %s identifier", position)
		}
	}
	return nil
}

// semverCharDebug is the first character of text as Rust's {:?} writes a
// char.
func semverCharDebug(text string) string {
	r, _ := utf8.DecodeRuneInString(text)
	return strconv.QuoteRune(r)
}

// semverComparePrecedence is semver's cmp_precedence for two versions as
// semverVersion.String writes them: build metadata takes no part.
func semverComparePrecedence(a, b string) int {
	left, _, leftErr := parseSemver(a)
	right, _, rightErr := parseSemver(b)
	if leftErr != nil || rightErr != nil {
		return strings.Compare(a, b)
	}
	if ordering := cmp.Compare(left.major, right.major); ordering != 0 {
		return ordering
	}
	if ordering := cmp.Compare(left.minor, right.minor); ordering != 0 {
		return ordering
	}
	if ordering := cmp.Compare(left.patch, right.patch); ordering != 0 {
		return ordering
	}
	return semverComparePre(left.pre, right.pre)
}

// semverComparePre orders pre-releases: none is greater than any, then by
// identifier, numeric ones numerically and below alphanumeric ones.
func semverComparePre(a, b string) int {
	switch {
	case a == b:
		return 0
	case a == "":
		return 1
	case b == "":
		return -1
	}
	left, right := strings.Split(a, "."), strings.Split(b, ".")
	for i := 0; i < len(left) && i < len(right); i++ {
		leftNumeric, rightNumeric := isASCIIDigits(left[i]), isASCIIDigits(right[i])
		var ordering int
		switch {
		case leftNumeric && rightNumeric:
			ordering = cmp.Compare(len(left[i]), len(right[i]))
			if ordering == 0 {
				ordering = strings.Compare(left[i], right[i])
			}
		case leftNumeric:
			ordering = -1
		case rightNumeric:
			ordering = 1
		default:
			ordering = strings.Compare(left[i], right[i])
		}
		if ordering != 0 {
			return ordering
		}
	}
	return cmp.Compare(len(left), len(right))
}

func isASCIIDigits(text string) bool {
	for i := 0; i < len(text); i++ {
		if text[i] < '0' || text[i] > '9' {
			return false
		}
	}
	return text != ""
}

func isASCIIHex(text string) bool {
	for i := 0; i < len(text); i++ {
		c := text[i]
		if !(c >= '0' && c <= '9' || c >= 'a' && c <= 'f' || c >= 'A' && c <= 'F') {
			return false
		}
	}
	return true
}

// parseRustInt64 is Rust's str::parse::<i64>: an optional sign and decimal
// digits, nothing else.
func parseRustInt64(text string) (int64, error) {
	unsigned := strings.TrimPrefix(text, "+")
	if !isASCIIDigits(strings.TrimPrefix(unsigned, "-")) {
		return 0, strconv.ErrSyntax
	}
	return strconv.ParseInt(unsigned, 10, 64)
}

// parseRustUint64 is Rust's str::parse for an unsigned integer: an optional
// `+` and decimal digits.
func parseRustUint64(text string) (uint64, error) {
	digits := strings.TrimPrefix(text, "+")
	if !isASCIIDigits(digits) {
		return 0, strconv.ErrSyntax
	}
	return strconv.ParseUint(digits, 10, 64)
}

// buildIdentityDebug formats text as Rust's {:?} does for a str.
func buildIdentityDebug(text string) string {
	var b strings.Builder
	b.WriteByte('"')
	for _, r := range text {
		switch r {
		case '"':
			b.WriteString(`\"`)
		case '\\':
			b.WriteString(`\\`)
		case '\n':
			b.WriteString(`\n`)
		case '\r':
			b.WriteString(`\r`)
		case '\t':
			b.WriteString(`\t`)
		case 0:
			b.WriteString(`\0`)
		default:
			if unicode.IsPrint(r) || r == ' ' {
				b.WriteRune(r)
			} else {
				fmt.Fprintf(&b, `\u{%x}`, r)
			}
		}
	}
	b.WriteByte('"')
	return b.String()
}
