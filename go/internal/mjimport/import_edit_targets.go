// Port of mj-import/src/import/edit_targets.rs.

package mjimport

// SessionEditTargetsFor is Rust's `session_edit_targets` (renamed: the type
// SessionEditTargets keeps its name): the Git roots and other directories a
// native session edited, outside its profile home.
func SessionEditTargetsFor(transcript *ClaudeTranscript, profileHome string) (SessionEditTargets, error) {
	return sessionEditTargetsWithScratchPrefixes(transcript, profileHome, scratchPrefixes())
}

// scratchPrefixes are directories whose repositories are throwaway
// workspaces rather than projects. A session that writes into one of them is
// still anchored on its own repository.
func scratchPrefixes() []string {
	panic("unported: mj-import/src/import/edit_targets.rs scratch_prefixes")
}

func sessionEditTargetsWithScratchPrefixes(
	transcript *ClaudeTranscript,
	profileHome string,
	scratchPrefixes []string,
) (SessionEditTargets, error) {
	panic("unported: mj-import/src/import/edit_targets.rs session_edit_targets_with_scratch_prefixes")
}

func isScratchRoot(root string, scratchPrefixes []string) bool {
	for _, prefix := range scratchPrefixes {
		if importPathStartsWith(root, prefix) {
			return true
		}
	}
	return false
}

// canonicalizeExistingAncestor canonicalizes the nearest existing ancestor of
// path and appends the rest of path to it; path itself when no ancestor can
// be canonicalized.
func canonicalizeExistingAncestor(path string) string {
	panic("unported: mj-import/src/import/edit_targets.rs canonicalize_existing_ancestor")
}

// editedDirectory is path when it is a directory, otherwise its parent.
func editedDirectory(path string) string {
	panic("unported: mj-import/src/import/edit_targets.rs edited_directory")
}

// GitRootForPath finds the worktree containing a path, or its nearest
// existing ancestor. false when that directory is in no Git repository.
func GitRootForPath(path string) (string, bool, error) {
	panic("unported: mj-import/src/import/edit_targets.rs git_root_for_path")
}
