// Port of the tests in mj-core/src/github_reference.rs.

package core

import (
	"slices"
	"testing"
)

func TestReferenceBoundariesKeepNumbersAndRepositoriesDistinct(t *testing.T) {
	references := GithubReferences("#12 #123 x#12 owner/repo#12 https://github.com/OWNER/repo/pull/12/files). #12x")
	var numbers []uint64
	for _, reference := range references {
		numbers = append(numbers, reference.Number)
	}
	if !slices.Equal(numbers, []uint64{12, 123, 12, 12}) {
		t.Fatalf("numbers = %v", numbers)
	}
	if references[2].Owner == nil || *references[2].Owner != "owner" {
		t.Fatalf("owner = %v", references[2].Owner)
	}
	if references[3].Repo == nil || *references[3].Repo != "repo" {
		t.Fatalf("repo = %v", references[3].Repo)
	}
}
