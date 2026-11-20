// Port of the tests in mj-core/src/github_item.rs.

package core

import (
	"encoding/json"
	"slices"
	"strings"
	"testing"
)

func githubItemTestEvidence() GithubItemEvidence {
	return GithubItemEvidence{
		Item: GithubItem{
			Repo:   "brokkai/mjolnir",
			Kind:   GithubItemKindPullRequest,
			Number: 42,
			Title:  "Add mailbox support",
			Body:   "Add mailbox support",
			Author: "agent",
			URL:    "https://github.com/brokkai/mjolnir/pull/42",
		},
		Session: SessionContext{
			Conversation: "User: Implement mailbox support\nAgent: Checking the worker\n",
			Files:        []string{"mj-worker/src/lib.rs"},
			References:   []string{"#42"},
		},
	}
}

func TestItemMentionsResolveBareReferencesWithoutCrossingRepositories(t *testing.T) {
	item := GithubItem{
		Repo:   "owner/repo",
		Kind:   GithubItemKindIssue,
		Number: 42,
		Title:  "Fix #7642",
		URL:    "https://github.com/owner/repo/issues/42",
	}
	var session SessionContext
	for _, line := range []string{"pending #7642", "other/repo#42", "owner/repo#42", "pending #7642"} {
		for _, reference := range GithubReferences(line) {
			session.MentionCandidates = append(session.MentionCandidates, GithubMentionCandidate{Reference: reference, Line: line})
		}
	}
	// A multi-repository session's bare #7642 names no repository.
	ambiguous := session.ForItem(&item, false)
	if !slices.Equal(ambiguous.Mentions, []string{"owner/repo#42"}) {
		t.Fatalf("ambiguous mentions = %q", ambiguous.Mentions)
	}
	selected := session.ForItem(&item, true)
	if !slices.Equal(selected.Mentions, []string{"pending #7642", "owner/repo#42"}) {
		t.Fatalf("selected mentions = %q", selected.Mentions)
	}
	encoded, err := json.Marshal(selected)
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(encoded), "mention_candidates") {
		t.Fatalf("serialized mention candidates: %s", encoded)
	}
}

func TestBoundedRequestMarksTruncationWithoutSplittingUTF8(t *testing.T) {
	typical := githubItemTestEvidence()
	length, err := upstreamLen(&typical)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("typical v2 request: %d bytes", length)
	maximal := typical
	maximal.Item.Repo = strings.Repeat("r", 256)
	maximal.Item.Title = strings.Repeat("t", 1024)
	maximal.Item.Author = strings.Repeat("a", 256)
	maximal.Item.URL = strings.Repeat("u", 2048)
	maximal.Item.Number = 9_007_199_254_740_991
	maximal.Item.Body = strings.Repeat("x", ItemBodyBytes-1) + "🦀"
	maximal.Session = SessionContext{
		Conversation: strings.Repeat("x", ConversationBytes),
		Files:        slices.Repeat([]string{strings.Repeat("x", FileBytes)}, MaxFiles),
		References:   slices.Repeat([]string{strings.Repeat("x", ReferenceBytes)}, MaxReferences),
	}
	bounded, err := maximal.BoundedForRequest()
	if err != nil {
		t.Fatal(err)
	}
	if bounded.Item.Body != strings.Repeat("x", ItemBodyBytes-1)+bodyTruncationMarker {
		t.Fatalf("bounded body = %q", bounded.Item.Body)
	}
	length, err = upstreamLen(&bounded)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("maximal v2 request: %d bytes", length)
	if length > GithubItemMaxBodyBytes {
		t.Fatalf("maximal request is %d bytes", length)
	}
	escaped := bounded
	escaped.Item.Repo = strings.Repeat(`"`, 256)
	escaped.Item.Title = strings.Repeat(`"`, 1024)
	escaped.Item.Author = strings.Repeat(`"`, 256)
	escaped.Item.URL = strings.Repeat(`"`, 2048)
	escaped.Item.Body = strings.Repeat(`"`, ItemBodyBytes) + bodyTruncationMarker
	escaped.Session.Conversation = strings.Repeat(`"`, ConversationBytes)
	escaped.Session.Files = slices.Repeat([]string{strings.Repeat(`"`, FileBytes)}, MaxFiles)
	escaped.Session.References = slices.Repeat([]string{strings.Repeat(`"`, ReferenceBytes)}, MaxReferences)
	if err := escaped.Validate(); err != nil {
		t.Fatal(err)
	}
	length, err = upstreamLen(&escaped)
	if err != nil {
		t.Fatal(err)
	}
	t.Logf("maximal escaped v2 request: %d bytes", length)
	// Individual UTF-8 budgets do not bound JSON's six-byte control escapes.
	// The complete envelope must still reject oversized combinations.
	escaped.Item.Body = strings.Repeat("\x00", ItemBodyBytes)
	if length, _ := upstreamLen(&escaped); length <= GithubItemMaxBodyBytes {
		t.Fatalf("control escapes fit in %d bytes", length)
	}
	if escaped.Validate() == nil {
		t.Fatal("oversized escaped request validated")
	}
	serialized, err := json.Marshal(bounded.UpstreamBody())
	if err != nil {
		t.Fatal(err)
	}
	if strings.Index(string(serialized), `"item"`) >= strings.Index(string(serialized), `"session"`) {
		t.Fatalf("item does not precede session: %s", serialized)
	}
}

func TestEvidenceSerializationParsesOnlyTypedV2Answers(t *testing.T) {
	parse := func(text string) (GithubItemVerdict, error) {
		var response any
		if err := json.Unmarshal([]byte(text), &response); err != nil {
			t.Fatal(err)
		}
		return GithubItemVerdictParse(response)
	}
	verdict, err := parse(`{"answers":{"interested":{"type":"noul","noul":0.5}}}`)
	if err != nil || !verdict.Interested {
		t.Fatalf("verdict = %v, %v", verdict, err)
	}
	for _, response := range []string{
		`{"answers":{"interested":{"type":"choice","choice":"yes"}}}`,
		`{"answers":{"interested":{"type":"noul","noul":1.1}}}`,
		`{"answers":{"interested":{"type":"noul","noul":0.1},"created":{"type":"noul","noul":0.2}}}`,
	} {
		if _, err := parse(response); err == nil {
			t.Fatalf("accepted %s", response)
		}
	}
}
