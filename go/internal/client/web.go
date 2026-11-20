// Port of mj-client/src/web.rs.

// Web-viewer data shared by Mjolnir's daemon and control surfaces.

package client

import (
	"fmt"
	"net/netip"
)

// WebBrowserTranscript is the conversation shape the phone reads. The chat
// layer projects its entries into this; the browser API owns the wire form.
//
// Rust's BrowserTranscript. Renamed by the package's collision rule: the
// transcript module's browser_transcript function holds the name.
type WebBrowserTranscript struct {
	LatestSeq uint64 `json:"latest_seq"`
	// PresentationKey is an opaque Rich-presentation topology key. A browser
	// sends the key it last rendered with its next delta request; a mismatch
	// means an append-only feed may contain rows the current presentation has
	// removed.
	PresentationKey string `json:"presentation_key"`
	// WindowStartSeq is the cursor boundary below which a client must replace
	// its feed. Usually this is the oldest retained entry, but presentation
	// coalescing may advance it so an append-only client drops a marker hidden
	// by a newer entry.
	WindowStartSeq uint64 `json:"window_start_seq"`
	Reset          bool   `json:"reset"`
	// Entries are shared with the projector so publication retains one copy
	// of each row.
	Entries []*BrowserTranscriptEntry `json:"entries"`
}

func (v WebBrowserTranscript) MarshalJSON() ([]byte, error) {
	type plain WebBrowserTranscript
	return daemonMarshalStruct(plain(v))
}

type BrowserTranscriptEntry struct {
	// CommandID is the submission identity for replacing a client-local
	// pending row.
	CommandID *string `json:"command_id,omitempty"`
	// StableID is the stable identity for multiple messages delivered at the
	// same relay ordinal.
	StableID     *string `json:"stable_id,omitempty"`
	ID           uint64  `json:"id"`
	UpdatedSeq   uint64  `json:"updated_seq"`
	Role         string  `json:"role"`
	Label        string  `json:"label"`
	RecordedAtMs *int64  `json:"recorded_at_ms"`
	// ToolEndedAtMs is when a finished tool call ended. Nil for every other
	// entry.
	ToolEndedAtMs *int64 `json:"tool_ended_at_ms,omitempty"`
	// Generated marks a user-role prompt that Mjolnir submitted itself, such
	// as a retry, an automatic continuation or another session's message. The
	// server decides this once (ChatEntry's is_user_authored); the viewer
	// must not repeat the rules.
	Generated bool     `json:"generated,omitempty"`
	Lines     []string `json:"lines"`
	// MailboxEvents are mailbox events split into a header and report
	// continuation lines, from the same bounded text as Lines. Each event is
	// a separate disclosure.
	MailboxEvents [][]string `json:"mailbox_events,omitempty"`
	// Glyph is the glyph the terminal draws for this role, so both surfaces
	// read alike without the browser keeping a second copy of the mapping.
	// Taken from the same entry visual the terminal renders from.
	Glyph string `json:"glyph"`
	// Tone is the semantic colour name, not a colour. The stylesheet decides
	// what `agent` or `failed` looks like; this says which one applies.
	Tone string `json:"tone"`
	// ToolStatus is a tool call's state, for a tool entry. Nil for every
	// other role.
	ToolStatus *string `json:"tool_status,omitempty"`
	// Diffstats are the changed files a tool reported, as data rather than as
	// extra lines appended to Lines. The terminal formats these for a
	// terminal; a browser re-parsing that formatting is how the phone came to
	// render every diffstat as one unsplit path.
	Diffstats []BrowserDiffStat `json:"diffstats,omitempty"`
}

func (v BrowserTranscriptEntry) MarshalJSON() ([]byte, error) {
	type plain BrowserTranscriptEntry
	return daemonMarshalStruct(plain(v))
}

// Identity is the entry's stable id, or its ordinal when it has none.
func (e BrowserTranscriptEntry) Identity() string {
	if e.StableID != nil {
		return *e.StableID
	}
	return fmt.Sprintf("ordinal:%d", e.ID)
}

// BrowserDiffStat is one file a tool changed, and by how much.
type BrowserDiffStat struct {
	Path       string `json:"path"`
	Insertions uint32 `json:"insertions"`
	Deletions  uint32 `json:"deletions"`
}

// WebViewerAccess is how a client reaches the web viewer. Externally tagged
// with the Rust variant names. String is Rust's Debug, which redacts the
// viewer credentials.
type WebViewerAccess interface {
	isWebViewerAccess()
	String() string
}

type WebViewerAccessStarting struct{}

type WebViewerAccessReady struct {
	ViewerURL      string  `json:"viewer_url"`
	ViewerCode     string  `json:"viewer_code"`
	QrLoginURL     *string `json:"qr_login_url"`
	FallbackReason *string `json:"fallback_reason"`
	// LocalAddress is the address a client on the daemon's host can use to
	// reach this listener. Published only after bind, with wildcard IPs
	// replaced by loopback.
	LocalAddress *netip.AddrPort `json:"local_address,omitempty" serde:"default"`
	// CertificateSha256 is the lowercase hex SHA-256 of the DER certificate
	// the viewer serves when that certificate is one the operator configured
	// (often self-signed). Local clients trust exactly this certificate
	// instead of a CA chain. Nil over plain HTTP and for publicly trusted
	// Tailscale certificates, which renew in place.
	CertificateSha256 *string `json:"certificate_sha256" serde:"default"`
}

type WebViewerAccessFailed struct {
	Address      netip.AddrPort `json:"address"`
	Message      string         `json:"message"`
	PortConflict bool           `json:"port_conflict"`
}

type WebViewerAccessUnavailable struct {
	Message string
}

func (WebViewerAccessStarting) isWebViewerAccess()    {}
func (WebViewerAccessReady) isWebViewerAccess()       {}
func (WebViewerAccessFailed) isWebViewerAccess()      {}
func (WebViewerAccessUnavailable) isWebViewerAccess() {}

func (WebViewerAccessStarting) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("Starting", nil)
}

func (v WebViewerAccessReady) MarshalJSON() ([]byte, error) {
	type plain WebViewerAccessReady
	return sessionExternalStruct("Ready", plain(v))
}

func (v WebViewerAccessFailed) MarshalJSON() ([]byte, error) {
	type plain WebViewerAccessFailed
	return sessionExternalStruct("Failed", plain(v))
}

func (v WebViewerAccessUnavailable) MarshalJSON() ([]byte, error) {
	body, err := daemonMarshalJSON(v.Message)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("Unavailable", body)
}

// UnmarshalWebViewerAccessJSON decodes serde's externally tagged form.
func UnmarshalWebViewerAccessJSON(data []byte) (WebViewerAccess, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "Starting":
		return WebViewerAccessStarting{}, nil
	case "Ready":
		return sessionExternalVariant[WebViewerAccessReady, WebViewerAccess](body)
	case "Failed":
		return sessionExternalVariant[WebViewerAccessFailed, WebViewerAccess](body)
	case "Unavailable":
		var v WebViewerAccessUnavailable
		if err := daemonDecodeValue(body, &v.Message); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"Starting", "Ready", "Failed", "Unavailable"})
}

func (WebViewerAccessStarting) String() string { return "Starting" }

func (v WebViewerAccessReady) String() string {
	reason := "None"
	if v.FallbackReason != nil {
		reason = fmt.Sprintf("Some(%q)", *v.FallbackReason)
	}
	return fmt.Sprintf("Ready { viewer_url: %q, credentials: \"[redacted]\", fallback_reason: %s }", v.ViewerURL, reason)
}

func (v WebViewerAccessFailed) String() string {
	return fmt.Sprintf("Failed { address: %s, message: %q, port_conflict: %t }", v.Address, v.Message, v.PortConflict)
}

func (v WebViewerAccessUnavailable) String() string {
	return fmt.Sprintf("Unavailable(%q)", v.Message)
}

// WebListenerProcess is the identity shown before an explicit stop request
// and checked again before signalling.
type WebListenerProcess struct {
	Pid                uint32  `json:"pid"`
	Name               string  `json:"name"`
	Executable         string  `json:"executable"`
	StartedAt          uint64  `json:"started_at"`
	StopDisabledReason *string `json:"stop_disabled_reason"`
}

func (v WebListenerProcess) MarshalJSON() ([]byte, error) {
	type plain WebListenerProcess
	return daemonMarshalStruct(plain(v))
}

func (v *WebListenerProcess) UnmarshalJSON(data []byte) error {
	type plain WebListenerProcess
	return daemonDecodeStruct(data, (*plain)(v), false)
}

// WebViewerRecovery is how the person asked to recover the web viewer.
// Externally tagged with the Rust variant names.
type WebViewerRecovery interface {
	isWebViewerRecovery()
}

type WebViewerRecoveryRetry struct{}

type WebViewerRecoveryAnotherPort struct{}

type WebViewerRecoveryStopAndRetry struct {
	Process WebListenerProcess
}

func (WebViewerRecoveryRetry) isWebViewerRecovery()        {}
func (WebViewerRecoveryAnotherPort) isWebViewerRecovery()  {}
func (WebViewerRecoveryStopAndRetry) isWebViewerRecovery() {}

func (WebViewerRecoveryRetry) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("Retry", nil)
}

func (WebViewerRecoveryAnotherPort) MarshalJSON() ([]byte, error) {
	return daemonExternalJSON("AnotherPort", nil)
}

func (v WebViewerRecoveryStopAndRetry) MarshalJSON() ([]byte, error) {
	body, err := daemonMarshalJSON(v.Process)
	if err != nil {
		return nil, err
	}
	return daemonExternalJSON("StopAndRetry", body)
}

// UnmarshalWebViewerRecoveryJSON decodes serde's externally tagged form.
func UnmarshalWebViewerRecoveryJSON(data []byte) (WebViewerRecovery, error) {
	name, body, err := daemonSplitExternal(data)
	if err != nil {
		return nil, err
	}
	switch name {
	case "Retry":
		return WebViewerRecoveryRetry{}, nil
	case "AnotherPort":
		return WebViewerRecoveryAnotherPort{}, nil
	case "StopAndRetry":
		var v WebViewerRecoveryStopAndRetry
		if err := daemonDecodeRequired(body, "StopAndRetry", &v.Process); err != nil {
			return nil, err
		}
		return v, nil
	}
	return nil, daemonUnknownVariant(name, []string{"Retry", "AnotherPort", "StopAndRetry"})
}
