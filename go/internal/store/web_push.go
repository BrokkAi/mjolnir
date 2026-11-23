// Port of mj-store/src/web_push.rs.

// Daemon-owned encrypted Web Push delivery. Durable intents precede HTTPS.

package store

import (
	"bytes"
	"context"
	"crypto/aes"
	"crypto/cipher"
	"crypto/ecdh"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/hkdf"
	"crypto/rand"
	"crypto/sha256"
	"crypto/x509"
	"encoding/base64"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"math"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
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
	type outcome struct {
		value T
		err   error
	}
	handle := core.SpawnBlocking(func() outcome {
		value, err := f()
		return outcome{value, err}
	})
	result, err := handle.Join(ctx)
	if err != nil {
		var zero T
		if ctxErr := ctx.Err(); ctxErr != nil && errors.Is(err, ctxErr) {
			return zero, err
		}
		return zero, fmt.Errorf("Web Push storage task panicked: %w", err)
	}
	return result.value, result.err
}

// webPushBase64 is base64's URL_SAFE_NO_PAD engine: no padding, and
// non-zero trailing bits refused.
var webPushBase64 = base64.RawURLEncoding.Strict()

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
	return databaseMarshalJSON(struct {
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
	endpoint, err := url.Parse(s.Endpoint)
	if err != nil {
		return err
	}
	if !endpoint.IsAbs() {
		return errors.New("relative URL without a base")
	}
	_, hasPassword := endpoint.User.Password()
	if endpoint.Scheme != "https" || endpoint.Hostname() == "" || endpoint.User.Username() != "" || hasPassword {
		return errors.New("push endpoint must be HTTPS")
	}
	key, err := webPushBase64.DecodeString(s.Keys.P256dh)
	if err != nil {
		return err
	}
	if len(key) != 65 || key[0] != 4 {
		return errors.New("invalid push public key")
	}
	auth, err := webPushBase64.DecodeString(s.Keys.Auth)
	if err != nil {
		return err
	}
	if len(auth) != 16 {
		return errors.New("invalid push authentication secret")
	}
	// The ECDH also validates that the point lies on P-256.
	_, err = Encrypt(s, nil)
	return err
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
	path := filepath.Join(core.DataDir(), "web-push-vapid.pk8")
	directory := filepath.Dir(path)
	if err := os.MkdirAll(directory, 0o777); err != nil {
		return nil, err
	}
	if _, err := os.Stat(path); errors.Is(err, os.ErrNotExist) {
		key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
		if err != nil {
			return nil, errors.New("generate VAPID key")
		}
		document, err := x509.MarshalPKCS8PrivateKey(key)
		if err != nil {
			return nil, errors.New("generate VAPID key")
		}
		// Atomic no-clobber publication also handles concurrent public-key
		// requests.
		if err := webPushPublishNoClobber(directory, path, document); err != nil {
			return nil, err
		}
	}
	if err := core.SyncDirectory(directory); err != nil {
		return nil, err
	}
	document, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	parsed, err := x509.ParsePKCS8PrivateKey(document)
	if err != nil {
		return nil, errors.New("read VAPID key")
	}
	key, ok := parsed.(*ecdsa.PrivateKey)
	if !ok || key.Curve != elliptic.P256() {
		return nil, errors.New("read VAPID key")
	}
	return &Vapid{key: key}, nil
}

// webPushPublishNoClobber writes document to a temporary file beside path,
// syncs it, and links it to path unless path already exists (tempfile's
// persist_noclobber). A path that appeared meanwhile is kept.
func webPushPublishNoClobber(directory, path string, document []byte) error {
	file, err := os.CreateTemp(directory, ".tmp")
	if err != nil {
		return err
	}
	temporary := file.Name()
	defer os.Remove(temporary)
	if _, err := file.Write(document); err != nil {
		file.Close()
		return err
	}
	if err := file.Sync(); err != nil {
		file.Close()
		return err
	}
	if err := file.Close(); err != nil {
		return err
	}
	if err := os.Link(temporary, path); err != nil && !errors.Is(err, os.ErrExist) {
		return err
	}
	return nil
}

// Public is the uncompressed public key, unpadded URL-safe base64.
func (v *Vapid) Public() string {
	public, err := v.key.PublicKey.ECDH()
	if err != nil {
		panic(fmt.Sprintf("VAPID public key: %v", err))
	}
	return webPushBase64.EncodeToString(public.Bytes())
}

// Authorization is the VAPID Authorization header for a push to endpoint.
func (v *Vapid) Authorization(endpoint string) (string, error) {
	origin, err := webPushOrigin(endpoint)
	if err != nil {
		return "", err
	}
	// RFC 8292 permits at most 24 hours; use that documented validity.
	claims, err := databaseMarshalJSON(struct {
		Aud string `json:"aud"`
		Exp int64  `json:"exp"`
		Sub string `json:"sub"`
	}{origin, time.Now().Unix() + 24*60*60, "https://github.com/BrokkAi/mjolnir"})
	if err != nil {
		return "", err
	}
	signed := webPushBase64.EncodeToString([]byte(`{"typ":"JWT","alg":"ES256"}`)) + "." + webPushBase64.EncodeToString(claims)
	digest := sha256.Sum256([]byte(signed))
	r, s, err := ecdsa.Sign(rand.Reader, v.key, digest[:])
	if err != nil {
		return "", errors.New("sign VAPID JWT")
	}
	// ECDSA_P256_SHA256_FIXED: r and s as two 32-byte big-endian integers.
	signature := make([]byte, 64)
	r.FillBytes(signature[:32])
	s.FillBytes(signature[32:])
	return fmt.Sprintf("vapid t=%s.%s, k=%s", signed, webPushBase64.EncodeToString(signature), v.Public()), nil
}

// webPushOrigin is the URL's origin as the url crate serializes it:
// scheme, host and any non-default port.
func webPushOrigin(endpoint string) (string, error) {
	parsed, err := url.Parse(endpoint)
	if err != nil {
		return "", err
	}
	if !parsed.IsAbs() {
		return "", errors.New("relative URL without a base")
	}
	defaults := map[string]string{"http": "80", "https": "443", "ws": "80", "wss": "443", "ftp": "21"}
	defaultPort, special := defaults[parsed.Scheme]
	if !special || parsed.Hostname() == "" {
		return "null", nil
	}
	host := strings.ToLower(parsed.Hostname())
	if strings.Contains(host, ":") {
		host = "[" + host + "]"
	}
	origin := parsed.Scheme + "://" + host
	if port := parsed.Port(); port != "" && port != defaultPort {
		origin += ":" + port
	}
	return origin, nil
}

// expand is HKDF-SHA256: extract with salt, then expand info to length
// bytes. (Rust's Length type adapts ring's hkdf::KeyType; Go's crypto/hkdf
// takes the length directly.)
func expand(salt, input, info []byte, length int) ([]byte, error) {
	okm, err := hkdf.Key(sha256.New, input, salt, string(info), length)
	if err != nil {
		return nil, errors.New("HKDF expand")
	}
	return okm, nil
}

// Encrypt is RFC 8291 aes128gcm encryption of payload to subscription.
func Encrypt(subscription *Subscription, payload []byte) ([]byte, error) {
	private, err := ecdh.P256().GenerateKey(rand.Reader)
	if err != nil {
		return nil, errors.New("push ECDH key")
	}
	public := private.PublicKey().Bytes()
	receiver, err := webPushBase64.DecodeString(subscription.Keys.P256dh)
	if err != nil {
		return nil, err
	}
	peer, err := ecdh.P256().NewPublicKey(receiver)
	if err != nil {
		return nil, errors.New("invalid push ECDH key")
	}
	shared, err := private.ECDH(peer)
	if err != nil {
		return nil, errors.New("invalid push ECDH key")
	}
	auth, err := webPushBase64.DecodeString(subscription.Keys.Auth)
	if err != nil {
		return nil, err
	}
	return encryptShared(shared, auth, receiver, public, payload)
}

func encryptShared(shared, auth, receiver, sender, payload []byte) ([]byte, error) {
	info := append([]byte("WebPush: info\x00"), receiver...)
	info = append(info, sender...)
	ikm, err := expand(auth, shared, info, 32)
	if err != nil {
		return nil, err
	}
	var salt [16]byte
	if _, err := rand.Read(salt[:]); err != nil {
		return nil, errors.New("push salt")
	}
	return encryptRecord(ikm, salt, sender, payload)
}

func encryptRecord(ikm []byte, salt [16]byte, sender, payload []byte) ([]byte, error) {
	key, err := expand(salt[:], ikm, []byte("Content-Encoding: aes128gcm\x00"), 16)
	if err != nil {
		return nil, err
	}
	nonce, err := expand(salt[:], ikm, []byte("Content-Encoding: nonce\x00"), 12)
	if err != nil {
		return nil, err
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return nil, errors.New("push AES key")
	}
	aead, err := cipher.NewGCM(block)
	if err != nil {
		return nil, errors.New("push AES key")
	}
	plaintext := append(append([]byte(nil), payload...), 2)
	record := aead.Seal(nil, nonce, plaintext, nil)
	// RFC 8291 requires one record. rs exceeds the final encrypted record
	// length.
	if uint64(len(record))+1 > math.MaxUint32 {
		return nil, errors.New("out of range integral type conversion attempted")
	}
	if len(sender) > math.MaxUint8 {
		return nil, errors.New("out of range integral type conversion attempted")
	}
	out := append([]byte(nil), salt[:]...)
	out = binary.BigEndian.AppendUint32(out, uint32(len(record)+1))
	out = append(out, byte(len(sender)))
	out = append(out, sender...)
	return append(out, record...), nil
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
	cards := runtime.Metadata.NotificationCards
	if cards == nil {
		return nil, false
	}
	// Attention supplies only worker availability. Card publication owns
	// notification eligibility, including ignored sessions and managed
	// children.
	unavailable := func(id string) bool {
		attention, ok := runtime.Metadata.SessionAttention.Get(id)
		if !ok {
			return false
		}
		switch attention {
		case core.SessionAttentionUnknown, core.SessionAttentionUnreachable, core.SessionAttentionFailed:
			return true
		}
		return false
	}
	entries := map[string]PushEntry{}
	for session, card := range cards.All() {
		if record, ok := runtime.Records.Get(session); ok && record.NotificationsIgnored {
			continue
		}
		holder := card.ItemDestination(session, len(card.Entries))
		for item, entry := range card.Entries {
			identity, ok := card.ItemIdentity(session, item)
			if !ok {
				continue
			}
			var messageID *string
			if found, ok := entry.Message.(core.EntryMessageFound); ok {
				id := found.Value
				messageID = &id
			}
			// The same stable message can be classified in two completions.
			// The card's oldest-first order gives that identity its first
			// position.
			if _, exists := entries[identity]; exists {
				continue
			}
			ordinal := entry.CompletedOrdinal
			entries[identity] = PushEntry{
				Session:          session,
				CompletedOrdinal: &ordinal,
				Paused:           unavailable(session),
				Holder:           card.ItemDestination(session, 0),
				Kind:             entry.Kind,
				MessageID:        messageID,
			}
		}
		// A form with an unknown raise time has no identity: it is not
		// pushable here.
		if card.Form != nil {
			if identity, ok := card.ItemIdentity(session, len(card.Entries)); ok {
				message := card.Form.Message
				entries[identity] = PushEntry{
					Session: session,
					Paused:  unavailable(session) || unavailable(holder),
					Holder:  holder,
					Kind:    core.NotificationKindNeedsInput,
					Text:    &message,
				}
			}
		}
	}
	return entries, true
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
	fallback := "Has a reply for you"
	if e.Kind == core.NotificationKindNeedsInput {
		fallback = "Needs your input"
	}
	title := "Mjolnir"
	if record, ok := runtime.Records.Get(e.Session); ok {
		title = record.ListedTitle()
	}
	body := ""
	if message != nil {
		body = *message
	}
	session := e.Session
	return Notice{
		Title:            title,
		Body:             phoneLine(body, fallback),
		SessionID:        &session,
		URL:              "/#conversation/" + e.Holder,
		Tag:              e.Session,
		Episode:          &identity,
		ObservedRevision: runtime.Revision,
	}
}
