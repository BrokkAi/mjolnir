// Port of the tests in mj-store/src/web_push.rs.

package store

import (
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"crypto/sha256"
	"encoding/json"
	"math/big"
	"strings"
	"testing"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func TestEachCardItemPushTargetsItsOwnSessionOrFormHolder(t *testing.T) {
	var runtime client.RuntimeProjection
	holder := "child"
	cards := core.NewSnapshotMap[string, client.NotificationCard]()
	cards.Insert("parent", client.NotificationCard{
		SinceMs: 1,
		Badge:   core.NotificationKindNeedsInput,
		Entries: []core.NotificationStackEntry{{
			CompletedAtMs:    1,
			CompletedOrdinal: 4,
			Kind:             core.NotificationKindNeedsInput,
			Message:          core.EntryMessageNone{},
		}},
		Dismissal: core.NotificationDismissalStack{Revision: 1},
		Holder:    &holder,
		Form: &client.CardForm{
			Source:     client.CardFormSourceElicitation,
			RaisedAtMs: 2,
			RequestID:  "r1",
			Message:    "form",
		},
	})
	runtime.Metadata.NotificationCards = &cards
	entries, ok := CardEntries(&runtime)
	if !ok {
		t.Fatal("published cards must yield entries")
	}
	var question, form *PushEntry
	for _, entry := range entries {
		entry := entry
		if entry.CompletedOrdinal != nil && *entry.CompletedOrdinal == 4 {
			question = &entry
		}
		if entry.CompletedOrdinal == nil {
			form = &entry
		}
	}
	if question == nil || form == nil {
		t.Fatalf("entries = %+v", entries)
	}
	if question.Holder != "parent" {
		t.Fatalf("question holder = %s", question.Holder)
	}
	if url := question.Notice(&runtime, "question", nil).URL; url != "/#conversation/parent" {
		t.Fatalf("question url = %s", url)
	}
	if form.Holder != "child" {
		t.Fatalf("form holder = %s", form.Holder)
	}
	if url := form.Notice(&runtime, "form", nil).URL; url != "/#conversation/child" {
		t.Fatalf("form url = %s", url)
	}
}

func webPushDecode(t *testing.T, value string) []byte {
	t.Helper()
	decoded, err := webPushBase64.DecodeString(value)
	if err != nil {
		t.Fatal(err)
	}
	return decoded
}

func TestRfc8291PublishedEncryptionVectorAndVapidSignatureVerify(t *testing.T) {
	// RFC 8291 Appendix A: independent, published interoperability fixture.
	receiver := webPushDecode(t, "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4")
	sender := webPushDecode(t, "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8")
	info := append([]byte("WebPush: info\x00"), receiver...)
	info = append(info, sender...)
	ikm, err := expand(
		webPushDecode(t, "BTBZMqHH6r4Tts7J_aSIgg"),
		webPushDecode(t, "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
		info,
		32,
	)
	if err != nil {
		t.Fatal(err)
	}
	if got := webPushBase64.EncodeToString(ikm); got != "S4lYMb_L0FxCeq0WhDx813KgSYqU26kOyzWUdsXYyrg" {
		t.Fatalf("ikm = %s", got)
	}
	var salt [16]byte
	copy(salt[:], webPushDecode(t, "DGv6ra1nlYgDCS1FRnbzlw"))
	encrypted, err := encryptRecord(ikm, salt, sender, []byte("When I grow up, I want to be a watermelon"))
	if err != nil {
		t.Fatal(err)
	}
	if got := webPushBase64.EncodeToString(encrypted[86:]); got != "8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ" {
		t.Fatalf("record = %s", got)
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	vapid := &Vapid{key: key}
	header, err := vapid.Authorization("https://push.example.com/send/device")
	if err != nil {
		t.Fatal(err)
	}
	token, ok := strings.CutPrefix(header, "vapid t=")
	if !ok {
		t.Fatalf("header = %s", header)
	}
	token, _, _ = strings.Cut(token, ",")
	parts := strings.Split(token, ".")
	signature := webPushDecode(t, parts[2])
	digest := sha256.Sum256([]byte(parts[0] + "." + parts[1]))
	r, s := new(big.Int).SetBytes(signature[:32]), new(big.Int).SetBytes(signature[32:])
	if len(signature) != 64 || !ecdsa.Verify(&key.PublicKey, digest[:], r, s) {
		t.Fatal("the VAPID signature does not verify")
	}
	var claims map[string]any
	if err := json.Unmarshal(webPushDecode(t, parts[1]), &claims); err != nil {
		t.Fatal(err)
	}
	if claims["aud"] != "https://push.example.com" {
		t.Fatalf("aud = %v", claims["aud"])
	}
	if exp, ok := claims["exp"].(float64); !ok || int64(exp) <= time.Now().Unix() {
		t.Fatalf("exp = %v", claims["exp"])
	}
}
