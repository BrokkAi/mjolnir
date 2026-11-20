// Port of the tests in mj-core/src/refusal.rs.

package core

import (
	"errors"
	"fmt"
	"testing"
)

// Hard-won: 796d055c: HTTP callers received a generic 500 instead of the refusal reason.
func TestARefusalSurvivesAddedContext(t *testing.T) {
	err := fmt.Errorf("start a phone session: %w",
		RefusalPrecondition("create a workspace first").Wrap(errors.New("relay path /home/someone/profile is missing")))
	refusal, ok := RefusalOf(err)
	if !ok {
		t.Fatal("the refusal is still on the chain")
	}
	if refusal.Message() != "create a workspace first" {
		t.Fatalf("message %q", refusal.Message())
	}
	if refusal.Kind() != RefusalKindPrecondition {
		t.Fatalf("kind %v", refusal.Kind())
	}

	source := fmt.Errorf("refresh capacity: %w", error(RefusalUnusable("no target named laptop is configured")))
	sourceRefusal, ok := RefusalOf(source)
	if !ok {
		t.Fatal("the refusal is found in an error source")
	}
	if sourceRefusal.Kind() != RefusalKindUnusable {
		t.Fatalf("kind %v", sourceRefusal.Kind())
	}
}

func TestAnUnmarkedErrorCarriesNoRefusal(t *testing.T) {
	err := fmt.Errorf("provision the session target: %w", errors.New("ssh host build-07 refused the connection"))
	if _, ok := RefusalOf(err); ok {
		t.Fatal("an unmarked error carries no refusal")
	}
}
