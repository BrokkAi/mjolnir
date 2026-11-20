// Port of mj-core/src/github_reference.rs.

// GitHub references shared by transcript readers.

package core

import "regexp"

type GithubReference struct {
	Owner  *string
	Repo   *string
	Number uint64
	Text   string
	// Byte offset of `Text` in the parsed string.
	Start int
}

var githubReferencePattern = regexp.MustCompile(`(?i)https://github\.com/([a-z0-9_-]+)/([a-z0-9_.-]+)/(?:issues|pull)/([0-9]+)|(?:([a-z0-9_-]+)/([a-z0-9_.-]+))?#([0-9]+)`)

func GithubReferences(text string) []GithubReference {
	panic("unported: mj-core/src/github_reference.rs github_references")
}
