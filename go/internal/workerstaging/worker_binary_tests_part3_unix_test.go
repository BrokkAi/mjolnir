//go:build unix

// Port of mj-worker-staging/src/worker_binary/tests.rs: the `#[cfg(unix)]`
// profile staging tests that follow links.

package workerstaging

import (
	"crypto/rand"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func makeFixtureLink(t *testing.T, target, link string) {
	t.Helper()
	if err := os.Symlink(target, link); err != nil {
		t.Fatal(err)
	}
}

// Hard-won: c607200: symlinked Claude settings and instructions were silently dropped
func TestStageClaudeProfileFollowsSymlinkedEntries(t *testing.T) {
	outside := t.TempDir()
	writeFixture(t, filepath.Join(outside, "settings.json"), []byte("{\"model\":\"opus\"}"))
	writeFixture(t, filepath.Join(outside, "CLAUDE.md"), []byte("# linked instructions\n"))
	skills := filepath.Join(outside, "skills")
	writeFixture(t, filepath.Join(skills, "review/SKILL.md"), []byte("review skill\n"))
	// A dangling link inside a copied tree must not fail staging.
	makeFixtureLink(t, filepath.Join(outside, "missing"), filepath.Join(skills, "dangling.md"))

	home := t.TempDir()
	makeFixtureLink(t, filepath.Join(outside, "settings.json"), filepath.Join(home, "settings.json"))
	makeFixtureLink(t, filepath.Join(outside, "CLAUDE.md"), filepath.Join(home, "CLAUDE.md"))
	makeFixtureLink(t, skills, filepath.Join(home, "skills"))

	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}

	for _, entry := range []struct{ relative, contents string }{
		{"settings.json", "{\"model\":\"opus\"}"},
		{"CLAUDE.md", "# linked instructions\n"},
		{"skills/review/SKILL.md", "review skill\n"},
	} {
		path := filepath.Join(staged, entry.relative)
		metadata, err := os.Lstat(path)
		if err != nil {
			t.Fatal(err)
		}
		if !metadata.Mode().IsRegular() {
			t.Fatalf("%s should be staged as a regular file", entry.relative)
		}
		if readFixture(t, path) != entry.contents {
			t.Fatalf("%s: %q", entry.relative, readFixture(t, path))
		}
	}
	if fixtureExists(filepath.Join(staged, "skills/dangling.md")) {
		t.Fatal("a dangling link was staged")
	}
}

// Launch finding R4-8: a profile home linked `skills/tufte-viz` from
// elsewhere, and the linked skill held a 2.2 MB demo. Staging copied both
// through the link; the session's worker then failed every skills poll on
// the large file, and the sync's own copy of the home did not read through
// the link at all. Stage and sync now agree, so the first sync neither fails
// nor removes the linked skill. A large file that compresses under the
// per-file limit is part of both trees; one that does not is left out of
// both.
// Hard-won: 17a0618: a linked oversized skill made worker sync fail repeatedly
func TestStagingAndSyncAgreeOnLinkedAndOversizedSkills(t *testing.T) {
	outside := t.TempDir()
	writeFixture(t, filepath.Join(outside, "viz/SKILL.md"), []byte("viz skill\n"))
	writeFixture(t, filepath.Join(outside, "viz/demos/sunspot-pretty.html"),
		[]byte(strings.Repeat("<tr><td>1749-01</td><td>96.7</td></tr>\n", 60_000)))
	incompressible := make([]byte, int(core.MaxSkillsFileBytes)+200_000)
	if _, err := rand.Read(incompressible); err != nil {
		t.Fatal(err)
	}
	writeFixture(t, filepath.Join(outside, "viz/demos/large.bin"), incompressible)
	home := t.TempDir()
	writeFixture(t, filepath.Join(home, "skills/review/SKILL.md"), []byte("review skill\n"))
	makeFixtureLink(t, filepath.Join(outside, "viz"), filepath.Join(home, "skills/viz"))

	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}
	if err := StageManagedSkills(profile.Kind, staged, core.SkillsScopeLocalhost); err != nil {
		t.Fatal(err)
	}

	expected, err := core.SessionSkills(profile.Kind, home, core.SkillsArchiveFormatGzip, core.SkillsScopeLocalhost)
	if err != nil {
		t.Fatal(err)
	}
	installed, err := core.CollectSkills(profile.Kind, staged)
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(installed, expected) {
		t.Fatal("the staged skills differ from what the sync pushes")
	}
	has := func(path string) bool {
		return slices.ContainsFunc(expected.Entries(), func(entry core.SkillsEntry) bool { return entry.Path == path })
	}
	if !has("skills/viz/SKILL.md") {
		t.Fatal("the linked skill is part of the canonical tree")
	}
	if !has("skills/viz/demos/sunspot-pretty.html") {
		t.Fatal("the large page compresses under the limit and is part of both sides")
	}
	if has("skills/viz/demos/large.bin") {
		t.Fatal("the oversized file is left out of both sides")
	}
}

func TestStageClaudeProfileSkipsDanglingAllowlistSymlinks(t *testing.T) {
	outside := t.TempDir()
	home := t.TempDir()
	makeFixtureLink(t, filepath.Join(outside, "missing"), filepath.Join(home, "CLAUDE.md"))
	writeFixture(t, filepath.Join(home, "settings.json"), []byte("{}"))
	staged := t.TempDir()
	profile := core.NewHarnessProfile(core.HarnessKindClaude, home)

	if err := StageProfile(&profile, staged); err != nil {
		t.Fatal(err)
	}

	if fixtureExists(filepath.Join(staged, "CLAUDE.md")) {
		t.Fatal("a dangling link was staged")
	}
	if !isFile(filepath.Join(staged, "settings.json")) {
		t.Fatal("the settings were not staged")
	}
}
