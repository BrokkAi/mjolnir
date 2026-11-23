// Port of mj-store/src/web_push.rs.

// Daemon-owned encrypted Web Push delivery. Durable intents precede HTTPS.

package store

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"encoding/json"
	"errors"
	"strings"
	"unicode/utf8"

	"github.com/BrokkAi/acp-go/jsonvalue"

	"github.com/BrokkAi/mjolnir/go/internal/client"
	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// wakeChannel is tokio's Notify for Web Push delivery: one stored permit,
// taken by the next waiter.
var wakeChannel = make(chan struct{}, 1)

// Wake raises the delivery loop's signal.
func Wake() {
	select {
	case wakeChannel <- struct{}{}:
	default:
	}
}

// WakeSignal is the signal Wake raises, for the delivery loop to wait on; a
// receive takes the stored permit.
func WakeSignal() <-chan struct{} {
	return wakeChannel
}

// Blocking runs f on a blocking task and waits for it, or for ctx to end.
func Blocking[T any](ctx context.Context, f func() (T, error)) (T, error) {
	panic("unported: mj-store/src/web_push.rs blocking")
}

// Subscription is a browser's push subscription, exactly as the Push API
// serializes it (serde deny_unknown_fields).
type Subscription struct {
	Endpoint       string           `json:"endpoint"`
	Keys           SubscriptionKeys `json:"keys"`
	ExpirationTime *float64         `json:"expirationTime"`
}

// MarshalJSON writes expirationTime as serde_json writes an f64.
func (s Subscription) MarshalJSON() ([]byte, error) {
	var expiration json.RawMessage = []byte("null")
	if s.ExpirationTime != nil {
		encoded, err := jsonvalue.Marshal(*s.ExpirationTime)
		if err != nil {
			return nil, err
		}
		expiration = encoded
	}
	return json.Marshal(struct {
		Endpoint       string           `json:"endpoint"`
		Keys           SubscriptionKeys `json:"keys"`
		ExpirationTime json.RawMessage  `json:"expirationTime"`
	}{s.Endpoint, s.Keys, expiration})
}

// UnmarshalJSON refuses unknown fields and requires endpoint and keys.
func (s *Subscription) UnmarshalJSON(data []byte) error {
	var raw struct {
		Endpoint       *string           `json:"endpoint"`
		Keys           *SubscriptionKeys `json:"keys"`
		ExpirationTime *float64          `json:"expirationTime"`
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&raw); err != nil {
		return err
	}
	switch {
	case raw.Endpoint == nil:
		return errors.New("missing field `endpoint`")
	case raw.Keys == nil:
		return errors.New("missing field `keys`")
	}
	*s = Subscription{Endpoint: *raw.Endpoint, Keys: *raw.Keys, ExpirationTime: raw.ExpirationTime}
	return nil
}

type SubscriptionKeys struct {
	P256dh string `json:"p256dh"`
	Auth   string `json:"auth"`
}

// UnmarshalJSON refuses unknown fields and requires both keys.
func (k *SubscriptionKeys) UnmarshalJSON(data []byte) error {
	var raw struct {
		P256dh *string `json:"p256dh"`
		Auth   *string `json:"auth"`
	}
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	if err := decoder.Decode(&raw); err != nil {
		return err
	}
	switch {
	case raw.P256dh == nil:
		return errors.New("missing field `p256dh`")
	case raw.Auth == nil:
		return errors.New("missing field `auth`")
	}
	*k = SubscriptionKeys{P256dh: *raw.P256dh, Auth: *raw.Auth}
	return nil
}

// Validate checks that the subscription can receive encrypted pushes.
func (s *Subscription) Validate() error {
	panic("unported: mj-store/src/web_push.rs Subscription::validate")
}

type Notice struct {
	Title            string  `json:"title"`
	Body             string  `json:"body"`
	SessionID        *string `json:"session_id"`
	URL              string  `json:"url"`
	Tag              string  `json:"tag"`
	Episode          *string `json:"episode"`
	ObservedRevision uint64  `json:"observed_revision"`
}

// UnmarshalJSON requires the fields serde requires; session_id, episode and
// observed_revision may be missing.
func (n *Notice) UnmarshalJSON(data []byte) error {
	var raw struct {
		Title            *string `json:"title"`
		Body             *string `json:"body"`
		SessionID        *string `json:"session_id"`
		URL              *string `json:"url"`
		Tag              *string `json:"tag"`
		Episode          *string `json:"episode"`
		ObservedRevision uint64  `json:"observed_revision"`
	}
	if err := json.Unmarshal(data, &raw); err != nil {
		return err
	}
	switch {
	case raw.Title == nil:
		return errors.New("missing field `title`")
	case raw.Body == nil:
		return errors.New("missing field `body`")
	case raw.URL == nil:
		return errors.New("missing field `url`")
	case raw.Tag == nil:
		return errors.New("missing field `tag`")
	}
	*n = Notice{
		Title:            *raw.Title,
		Body:             *raw.Body,
		SessionID:        raw.SessionID,
		URL:              *raw.URL,
		Tag:              *raw.Tag,
		Episode:          raw.Episode,
		ObservedRevision: raw.ObservedRevision,
	}
	return nil
}

// Vapid is the daemon's VAPID signing key (ECDSA P-256).
type Vapid struct {
	key *ecdsa.PrivateKey
}

// VapidLoad reads the daemon's VAPID key from the data directory, creating
// it on first use (Rust's Vapid::load).
func VapidLoad() (*Vapid, error) {
	panic("unported: mj-store/src/web_push.rs Vapid::load")
}

// Public is the uncompressed public key, unpadded URL-safe base64.
func (v *Vapid) Public() string {
	panic("unported: mj-store/src/web_push.rs Vapid::public")
}

// Authorization is the VAPID Authorization header for a push to endpoint.
func (v *Vapid) Authorization(endpoint string) (string, error) {
	panic("unported: mj-store/src/web_push.rs Vapid::authorization")
}

// expand is HKDF-SHA256: extract with salt, then expand info to length
// bytes. (Rust's Length type adapts ring's hkdf::KeyType; Go's crypto/hkdf
// takes the length directly.)
func expand(salt, input, info []byte, length int) ([]byte, error) {
	panic("unported: mj-store/src/web_push.rs expand")
}

// Encrypt is RFC 8291 aes128gcm encryption of payload to subscription.
func Encrypt(subscription *Subscription, payload []byte) ([]byte, error) {
	panic("unported: mj-store/src/web_push.rs encrypt")
}

func encryptShared(shared, auth, receiver, sender, payload []byte) ([]byte, error) {
	panic("unported: mj-store/src/web_push.rs encrypt_shared")
}

func encryptRecord(ikm []byte, salt [16]byte, sender, payload []byte) ([]byte, error) {
	panic("unported: mj-store/src/web_push.rs encrypt_record")
}

// phoneLine is the first line of text, trimmed and cut to 180 characters
// (a presentation length, not a transport or admission limit), or fallback
// when that is empty.
func phoneLine(text, fallback string) string {
	line, _, _ := strings.Cut(text, "\n")
	line = strings.TrimSpace(strings.TrimSuffix(line, "\r"))
	if utf8.RuneCountInString(line) > 180 {
		line = string([]rune(line)[:180])
	}
	if line == "" {
		return fallback
	}
	return line
}

// PushEntry is one pushable notification card item. Rust borrows its strings
// from the runtime projection (PushEntry<&str>) or owns them
// (PushEntry<String>); Go strings serve both.
type PushEntry struct {
	Session          string
	CompletedOrdinal *uint64
	Paused           bool
	Holder           string
	Kind             core.NotificationKind
	Text             *string
	MessageID        *string
}

// CardEntries is every pushable card item by its identity; false when the
// daemon publishes no notification cards.
func CardEntries(runtime *client.RuntimeProjection) (map[string]PushEntry, bool) {
	panic("unported: mj-store/src/web_push.rs card_entries")
}

// Owned is a copy that holds its own strings (Rust's PushEntry::owned).
func (e *PushEntry) Owned() PushEntry {
	owned := *e
	if e.CompletedOrdinal != nil {
		ordinal := *e.CompletedOrdinal
		owned.CompletedOrdinal = &ordinal
	}
	if e.Text != nil {
		text := *e.Text
		owned.Text = &text
	}
	if e.MessageID != nil {
		id := *e.MessageID
		owned.MessageID = &id
	}
	return owned
}

// Notice is the push this entry sends under identity, with message as its
// body when the message is known.
func (e *PushEntry) Notice(runtime *client.RuntimeProjection, identity string, message *string) Notice {
	panic("unported: mj-store/src/web_push.rs PushEntry::notice")
}
