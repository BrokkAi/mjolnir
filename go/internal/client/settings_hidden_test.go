// Port of the tests in mj-client/src/settings/hidden.rs.

package client

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestFastinoKeyIsHiddenAndTypedReferencesSurviveASettingsRoundTrip(t *testing.T) {
	var config core.Config
	if err := json.Unmarshal([]byte(fmt.Sprintf(`{"version": %d, "jev": {"fastino_key": "personal-key"}}`, core.ConfigVersion)), &config); err != nil {
		t.Fatal(err)
	}
	stored, shown := DraftOpen(&config)
	Hide(&shown)
	if !IsMarker(settingsPath(shown, "jev", "fastino_key")) {
		t.Fatal("the key is not hidden")
	}
	restored := settingsClone(shown)
	values, err := Restore(&restored, stored)
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(values, []string{"personal-key"}) {
		t.Fatalf("restored %v", values)
	}
	RehideRestored(&restored, shown)
	if !settingsEqual(restored, shown) {
		t.Fatal("rehiding does not give back the shown draft")
	}

	path := []string{"jev", "fastino_key"}
	cleared := settingsClone(shown)
	result, err := Apply(&cleared, path, SettingsEditRemove{Path: slices.Clone(path)})
	if err != nil {
		t.Fatal(err)
	}
	if !slices.Equal(result.Page, []string{"jev"}) {
		t.Fatalf("page %v", result.Page)
	}
	clearedConfig, err := ConfigFromDraft(cleared)
	if err != nil {
		t.Fatal(err)
	}
	if clearedConfig.Jev.FastinoKey != nil {
		t.Fatalf("key %v", clearedConfig.Jev.FastinoKey)
	}
	reference := `{ from_secret = "FASTINO" }`
	if _, err := Apply(&shown, path, SettingsEditSet{Path: slices.Clone(path), Text: &reference}); err != nil {
		t.Fatal(err)
	}
	page, err := RenderPage(shown, path)
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := page.Groups[0].Rows[0].Control.(SettingsControlText); !ok {
		t.Fatalf("control %#v", page.Groups[0].Rows[0].Control)
	}
	updatedConfig, err := ConfigFromDraft(shown)
	if err != nil {
		t.Fatal(err)
	}
	directory := t.TempDir()
	configPath := filepath.Join(directory, "config.toml")
	if err := config.SaveTo(configPath); err != nil {
		t.Fatal(err)
	}
	original := settingsDisplay(stored)
	encoded, err := json.Marshal(updatedConfig)
	if err != nil {
		t.Fatal(err)
	}
	updated := string(encoded)
	state := core.NewStateState()
	_, err = SaveAt(configPath, original, updated, &state)
	if err == nil || !strings.Contains(err.Error(), "jev.fastino_key") {
		t.Fatalf("error %v", err)
	}
	secretsPath := filepath.Join(directory, "secrets.toml")
	if err := os.WriteFile(secretsPath, []byte(`FASTINO = "private-marker" invalid`), 0o600); err != nil {
		t.Fatal(err)
	}
	_, err = SaveAt(configPath, original, updated, &state)
	if err == nil {
		t.Fatal("an invalid secrets file saved")
	}
	chain := err.Error()
	if !strings.Contains(chain, "secrets.toml") || !strings.Contains(chain, "at byte") || strings.Contains(chain, "private-marker") {
		t.Fatalf("chain %q", chain)
	}
	if err := os.WriteFile(secretsPath, []byte(`FASTINO = "my-provider-key"`), 0o600); err != nil {
		t.Fatal(err)
	}
	saved, err := SaveAt(configPath, original, updated, &state)
	if err != nil {
		t.Fatal(err)
	}
	savedValue, err := settingsToValue(saved)
	if err != nil {
		t.Fatal(err)
	}
	if !settingsEqual(settingsPath(savedValue, "jev", "fastino_key"), settingsJSON(t, `{"from_secret": "FASTINO"}`)) {
		t.Fatalf("key %s", settingsDisplay(settingsPath(savedValue, "jev", "fastino_key")))
	}

	saved.Jev.FastinoKey = core.EnvironmentValueLiteral{Value: "old-key "}
	if err := saved.SaveTo(configPath); err != nil {
		t.Fatal(err)
	}
	beforeBytes, err := json.Marshal(saved)
	if err != nil {
		t.Fatal(err)
	}
	saved.Jev.Enabled = false
	disabledBytes, err := json.Marshal(saved)
	if err != nil {
		t.Fatal(err)
	}
	disabled, err := SaveAt(configPath, string(beforeBytes), string(disabledBytes), &state)
	if err != nil {
		t.Fatal(err)
	}
	if disabled.Jev != saved.Jev {
		t.Fatalf("jev %+v, want %+v", disabled.Jev, saved.Jev)
	}
}

func TestPageActionPreservesTypedEnvironmentValuesThroughSave(t *testing.T) {
	stored := settingsJSON(t, `{
		"profiles": {"old": {"environment": {"TOKEN": "old-secret"}}},
		"targets": {"box": {"environment": {"KEY": "old-runtime"}}}
	}`)
	incoming := settingsClone(stored)
	Hide(&incoming)
	settingsSet(&incoming, "edited-secret", "profiles", "old", "environment", "TOKEN")
	settingsSet(&incoming, "new-secret", "profiles", "old", "environment", "NEW")
	settingsSet(&incoming, "new-runtime", "targets", "box", "environment", "NEW")
	actionResult := settingsClone(incoming)
	if _, err := Restore(&actionResult, stored); err != nil {
		t.Fatal(err)
	}
	RehideRestored(&actionResult, incoming)
	if got := settingsPath(actionResult, "profiles", "old", "environment", "TOKEN"); got != "edited-secret" {
		t.Errorf("TOKEN %v", got)
	}
	if got := settingsPath(actionResult, "profiles", "old", "environment", "NEW"); got != "new-secret" {
		t.Errorf("NEW %v", got)
	}
	if !IsMarker(settingsPath(actionResult, "targets", "box", "environment", "KEY")) {
		t.Error("the restored runtime key is not hidden again")
	}
	if _, err := Restore(&actionResult, stored); err != nil {
		t.Fatal(err)
	}
	if got := settingsPath(actionResult, "targets", "box", "environment", "NEW"); got != "new-runtime" {
		t.Errorf("runtime NEW %v", got)
	}
	if got := settingsPath(actionResult, "targets", "box", "environment", "KEY"); got != "old-runtime" {
		t.Errorf("runtime KEY %v", got)
	}
}
