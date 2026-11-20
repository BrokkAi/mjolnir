// Port of mj-core/src/skills/managed.rs.

// The host CLI skill Mjolnir installs into localhost sessions.
//
// The text lives in mj-core/assets/skills/<name>/SKILL.md and is embedded so
// a published build carries it; the only thing that varies between harnesses
// is the synced directory the entries are written under.

package core

import _ "embed"

//go:embed assets/skills/mj/SKILL.md
var managedSkillMD string

//go:embed assets/skills/mj/references/configuration.md
var managedSkillConfigurationMD string

// managed is the managed skill and its supporting files, in path order.
var managed = []struct {
	path string
	body string
}{
	{"SKILL.md", managedSkillMD},
	{"references/configuration.md", managedSkillConfigurationMD},
}

// MjSkillDirectory is the home-relative directory the managed mj skill is
// installed under for kind.
func MjSkillDirectory(kind HarnessKind) string {
	dirs := kind.SyncedSkillDirs()
	if len(dirs) == 0 {
		panic("every harness syncs at least one skills directory")
	}
	return dirs[0] + "/mj"
}

// ManagedSkills returns the files Mjolnir installs into localhost
// session-owned profile homes.
//
// Paths are home-relative, under the harness's first synced skills directory,
// and sorted, so the result can be merged into a collected archive directly.
func ManagedSkills(kind HarnessKind) []SkillsEntry {
	directory := MjSkillDirectory(kind)
	entries := make([]SkillsEntry, 0, len(managed))
	for _, file := range managed {
		entries = append(entries, SkillsEntry{Path: directory + "/" + file.path, Bytes: []byte(file.body)})
	}
	return entries
}
