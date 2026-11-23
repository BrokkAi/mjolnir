// Port of the tests of mj-worker/src/worker_runtime/unix/requests.rs.

//go:build unix

package worker

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func requestsCodexLogin(t *testing.T, refreshed string) []byte {
	t.Helper()
	encoded, err := json.Marshal(map[string]any{
		"auth_mode":    "chatgpt",
		"tokens":       map[string]any{"access_token": "access-" + refreshed},
		"last_refresh": refreshed,
	})
	if err != nil {
		t.Fatal(err)
	}
	return encoded
}

// Hard-won: #1485: an install used bytes captured before waiting for the harness refresh lock.
func TestCredentialInstallReportsTheNewerFileFoundAfterWaitingForNativeLock(t *testing.T) {
	home := t.TempDir()
	marker := filepath.Join(home, "auth.json")
	initial := requestsCodexLogin(t, "2026-09-25T20:00:00Z")
	incoming := requestsCodexLogin(t, "2026-09-25T21:00:00Z")
	latest := requestsCodexLogin(t, "2026-09-25T22:00:00Z")
	if err := os.WriteFile(marker, initial, 0o600); err != nil {
		t.Fatal(err)
	}
	held, err := core.CredentialWriteGuardAcquire(context.Background(), core.HarnessKindCodex, marker)
	if err != nil {
		t.Fatal(err)
	}
	endpoint := CredentialEndpoint{
		NoGithubCredentials: false,
		Harness:             core.HarnessKindCodex,
		Home:                home,
		Marker:              marker,
	}
	data := base64.StdEncoding.EncodeToString(incoming)
	relayRoot := t.TempDir()
	relay, err := DurableRelayOpen(relayRoot, "credential-lock-race", "fixture")
	if err != nil {
		t.Fatal(err)
	}
	identity := relay.loginIdentity
	fingerprint := core.CredentialFingerprint(initial)
	identity.observeFile(&fingerprint)
	install := core.Spawn(func() core.RelayResponseEnvelope {
		return credentialResponse(context.Background(), core.RelayRequestEnvelope{
			RequestID:       "credential-lock-race",
			ProtocolVersion: core.RelayProtocolVersion,
			Request:         core.RelayRequestInstallCredentials{Data: data},
		}, credentialEndpointResult{endpoint: endpoint}, endpoint.Home, identity)
	})
	select {
	case <-install.Done():
		t.Fatal("install must wait for the held destination lock")
	case <-time.After(200 * time.Millisecond):
	}
	if err := os.WriteFile(marker, latest, 0o600); err != nil {
		t.Fatal(err)
	}
	held.Release()

	response := testSupportWaitFor("the credential install", func() core.RelayResponseEnvelope {
		response, err := install.Join(context.Background())
		if err != nil {
			t.Error(err)
		}
		return response
	})
	ok, isOk := response.Body.(core.RelayResponseBodyOk)
	if !isOk {
		t.Fatalf("unexpected credential install response: %#v", response)
	}
	state, isState := ok.Payload.(core.RelayResponsePayloadCredentialState)
	if !isState {
		t.Fatalf("unexpected credential install response: %#v", response)
	}
	if !state.Present {
		t.Fatal("the installed login is absent")
	}
	if want := core.CredentialFingerprint(latest); state.Fingerprint != want {
		t.Fatalf("fingerprint %s, want %s", state.Fingerprint, want)
	}
	held2 := relay.OperationalState().HeldLoginFingerprint
	if held2 == nil || *held2 != state.Fingerprint {
		t.Fatalf("held login fingerprint %v", held2)
	}
	stored, err := os.ReadFile(marker)
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(stored, latest) {
		t.Fatal("the newer login was overwritten")
	}
}
