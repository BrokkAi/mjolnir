// Port of the tests in mj-core/src/usage/model_catalog.rs.

package core

import "testing"

// usageTestClaudeCatalog is claude2's discovered list on the live store,
// which offers both the opus alias and the older claude-opus-5.
func usageTestClaudeCatalog() ModelCatalogs {
	choice := func(value, name string) SessionConfigChoice {
		return SessionConfigChoice{Value: value, Name: name}
	}
	return ModelCatalogs{"claude": {
		choice("default", "Default (recommended)"),
		choice("opus", "Opus 5.5"),
		choice("claude-fable-5-1[1m]", "Fable 5.1"),
		choice("sonnet", "Sonnet 5"),
		choice("haiku", "Haiku 4.5"),
		choice("claude-opus-5", "Opus 5"),
		choice("claude-fable-5", "Fable 5"),
	}}
}

func TestClaudeAliasesResolveToTheModelTheCatalogNames(t *testing.T) {
	catalogs := usageTestClaudeCatalog()
	claude := "claude"
	for model, want := range map[string]string{
		"opus":          "claude-opus-5-5",
		"fable":         "claude-fable-5-1",
		"sonnet":        "claude-sonnet-5",
		"haiku":         "claude-haiku-4-5",
		"claude-opus-5": "claude-opus-5",
		// fable is not listed; it runs the family's newest listed version.
		// Unresolvable values and unknown profiles stay as recorded.
		"default":      "default",
		"custom-model": "custom-model",
	} {
		if got := catalogs.PricedModel(&claude, model); got != want {
			t.Fatalf("%s: got %s, want %s", model, got, want)
		}
	}
	if got := catalogs.PricedModel(nil, "opus"); got != "opus" {
		t.Fatal(got)
	}
	other := "other"
	if got := catalogs.PricedModel(&other, "opus"); got != "opus" {
		t.Fatal(got)
	}
}
