// Port of mj-worker-launch/src/local_profile_homes.rs.

// What earlier releases left in the profile homes on this machine.
//
// Before this release, a local bare session of most harnesses ran straight
// out of the person's profile home, such as `~/.codex`. Every session now
// runs from a staged home at `<worker root>/profile`, and the controller
// derives every path it uses (the launch configuration, a checkpoint's
// harness home, the project-memory replica) from that one rule.
//
// **Sessions still running from a profile home.** A session an earlier
// release started is still running from the profile home, with its login,
// its native transcript and its project-memory replica there. So that the
// rule still holds for such a session, daemon start puts a symbolic link at
// the staged path that points at the home the installed worker was started
// with. A restart with a refreshed launch configuration and a checkpoint then
// find the files the harness is using. Nothing is copied into or out of the
// profile home.
//
// Three writers must never go through the link: installing worker files,
// which would copy a stage over the person's own configuration, a restore,
// which would write a native session there, and the skills sync, which would
// replace their skills tree. Installing replaces the link with a new
// directory, the cleanup before a fresh restore unlinks it, and the
// credential sync leaves a session alone while its staged home is a link. The
// link goes when the worker root does, at close or when the harness is
// replaced in place; a resume then stages the session like any other.
//
// **Replicas of ended sessions.** Such a session kept Mjolnir's replica of
// its project memory at `<profile home>/projects/hel-<key>-<session>/`, and
// closing the session left it there. Daemon start leaves these alone: a
// profile home can be shared by several Mjolnir instances, and one instance
// cannot tell another's stopped session from an ended one.
//
// Platform parts: linkDirectory and RunningProcessArguments differ between
// Unix (local_profile_homes_unix.go) and other systems
// (local_profile_homes_nonunix.go); processCommandLines, sysinfo's process
// listing, is in local_profile_homes_processes_<os>.go.

package workerlaunch

import (
	"errors"
	"fmt"
	"io/fs"
	"log/slog"
	"os"
	"path/filepath"
	"strings"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// LinkedProfileHome is a session whose staged home was linked to the profile
// home its worker runs from.
type LinkedProfileHome struct {
	SessionID string
	// StagedHome is the staged path, now a symbolic link.
	StagedHome string
	// ProfileHome is the profile home the installed worker was started with.
	ProfileHome string
}

// LinkProfileHomesOfEarlierSessions links the staged home of every local bare
// session whose installed worker an earlier release started from a profile
// home. Returns what was linked, in session id order; a session that already
// has a staged home, or has no installed worker, is left alone.
func LinkProfileHomesOfEarlierSessions(state *core.StateState) []LinkedProfileHome {
	var linked []LinkedProfileHome
	for session := range state.Sessions.Values() {
		locator, ok := session.Target.(core.TargetLocatorLocalBare)
		if !ok {
			continue
		}
		// A harness with a nested home always ran from a per-session root
		// under the data directory.
		if session.HarnessKind.NestedHome() {
			continue
		}
		profileHome, ok, err := linkProfileHome(locator.WorkerRoot)
		switch {
		case err != nil:
			slog.Warn(
				fmt.Sprintf("could not link the staged home of a session an earlier release started: %v", err),
				"session_id", session.ID,
			)
		case ok:
			slog.Info(
				"this session's worker was started by an earlier release from the profile "+
					"home; its staged home now links there until the session is next staged",
				"session_id", session.ID,
				"profile_home", profileHome,
			)
			linked = append(linked, LinkedProfileHome{
				SessionID:   session.ID,
				StagedHome:  filepath.Join(locator.WorkerRoot, "profile"),
				ProfileHome: profileHome,
			})
		}
	}
	return linked
}

// linkProfileHome links `<workerRoot>/profile` to the home the installed
// worker was started with, when that home is not inside the worker root.
// Returns the linked profile home; ok is false when nothing was linked.
func linkProfileHome(workerRoot string) (profileHome string, ok bool, err error) {
	stagedHome := filepath.Join(workerRoot, "profile")
	if _, err := os.Lstat(stagedHome); err == nil {
		return "", false, nil
	} else if !errors.Is(err, fs.ErrNotExist) {
		return "", false, err
	}
	installedHome, ok, err := core.InstalledHarnessHome(filepath.Join(workerRoot, "launch.json"))
	if err != nil || !ok {
		return "", false, err
	}
	if info, err := os.Stat(installedHome); pathStartsWith(installedHome, workerRoot) || err != nil || !info.IsDir() {
		return "", false, nil
	}
	if err := linkDirectory(installedHome, stagedHome); err != nil {
		return "", false, err
	}
	return installedHome, true, nil
}

// pathStartsWith is Rust's `Path::starts_with`: whether base's components
// are a prefix of path's. Repeated separators and inner `.` components do
// not count; `..` is compared as written.
func pathStartsWith(path, base string) bool {
	left, right := pathComponents(path), pathComponents(base)
	if len(right) > len(left) {
		return false
	}
	for index := range right {
		if left[index] != right[index] {
			return false
		}
	}
	return true
}

// pathComponents is Rust's `Path::components` as text: the volume and the
// root first when there are any, then each name. A leading `.` of a
// relative path is kept; later `.` and empty names are dropped.
func pathComponents(path string) []string {
	var components []string
	volume := filepath.VolumeName(path)
	if volume != "" {
		components = append(components, volume)
	}
	rest := path[len(volume):]
	if rest != "" && os.IsPathSeparator(rest[0]) {
		components = append(components, string(filepath.Separator))
	}
	names := strings.FieldsFunc(rest, func(r rune) bool { return r < 0x80 && os.IsPathSeparator(uint8(r)) })
	for index, name := range names {
		if name == "." && (index > 0 || len(components) > 0 || !strings.HasPrefix(rest, ".")) {
			continue
		}
		components = append(components, name)
	}
	return components
}

// SessionHasAStagedHomeOfItsOwn is whether a session's staged home is a
// directory of its own, which the credential sync may write into.
//
// Every target but this machine stages a home for each session, and so does
// this release on this machine. A local session is left out while its staged
// home is a link to a profile home, or missing because an earlier release
// started its worker from a profile home that could not be linked.
func SessionHasAStagedHomeOfItsOwn(session *core.SessionRecord) bool {
	locator, ok := session.Target.(core.TargetLocatorLocalBare)
	if !ok {
		return true
	}
	if session.HarnessKind.NestedHome() {
		return true
	}
	info, err := os.Lstat(filepath.Join(locator.WorkerRoot, "profile"))
	return err == nil && info.IsDir()
}

// SessionSharesProfileLogin is whether a local session's login file is a link
// to its profile's own, which install makes for a harness that can share one
// (see core.HarnessKind.SharesLocalLogin). The credential sync has nothing to
// reconcile for such a session: both sides are one file.
//
// The file itself is checked rather than the harness, so a session an earlier
// release staged with a copy keeps its sync until it is staged again.
func SessionSharesProfileLogin(session *core.SessionRecord, harness core.HarnessKind) bool {
	locator, ok := session.Target.(core.TargetLocatorLocalBare)
	if !ok || !harness.SharesLocalLogin() {
		return false
	}
	marker := core.HarnessAuthenticationMarker(harness, filepath.Join(locator.WorkerRoot, "profile"))
	info, err := os.Lstat(marker)
	return err == nil && info.Mode()&fs.ModeSymlink != 0
}

// runningProcessArgumentsWithoutPs is RunningProcessArguments where there is
// no usable `ps`: each process's command line as the platform reports it,
// its arguments joined with spaces the way `ps` prints them. A process whose
// command line cannot be read lists as empty. An empty listing cannot be
// right, since it would not even hold this process, so it counts as one that
// failed (ok false, after a warning "could not list running processes: no
// command line was readable").
func runningProcessArgumentsWithoutPs() (arguments []string, ok bool) {
	arguments = processCommandLines()
	for _, argument := range arguments {
		if argument != "" {
			return arguments, true
		}
	}
	slog.Warn("could not list running processes: no command line was readable")
	return nil, false
}
