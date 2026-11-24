// Port of mj-worker-staging/src/worker_binary/profile_transfer.rs.

// Receiver-owned negotiation for staged profile files.

package workerstaging

import (
	"archive/tar"
	"bufio"
	"bytes"
	_ "embed"
	"io"
	"os"
	"slices"
	"time"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

// profileReceiver is Rust's RECEIVER: the Perl program that receives a
// negotiated profile on the target.
//
//go:embed data/worker_binary/profile_receiver.pl
var profileReceiver string

// profileReceiverProbe is Rust's PROBE: whether the target's Perl has every
// module the receiver uses.
const profileReceiverProbe = "perl -MDigest::SHA -MFcntl -MFile::Copy -MFile::Path -MFile::Temp -MJSON::PP -e 1"

// manifestEntry is one entry of the materialized profile, serialized in
// serde's field order.
type manifestEntry struct {
	// Path is the hex of the entry's path bytes: Unix filenames may contain
	// newlines or non-UTF-8 bytes.
	Path      string  `json:"path"`
	Directory bool    `json:"directory"`
	Mode      uint32  `json:"mode"`
	Size      uint64  `json:"size"`
	Key       *string `json:"key"`
}

// ProfileContentManifest is the content inventory for a staged profile
// snapshot. Keep the staging tree alive until transfer completes so claimed
// bodies match the recorded digest.
type ProfileContentManifest struct {
	source  string
	entries []manifestEntry
	// objects is Rust's BTreeMap: iterate it in key order (sortedObjectKeys).
	objects map[string]profileObject
	private []string
}

// profileObject is one shareable object's first path in the stage and its
// size.
type profileObject struct {
	path string
	size uint64
}

// ProfileContentManifestFromStaged is Rust's
// `ProfileContentManifest::from_staged`. Use only roots declared by the
// profile stager. Unknown entries travel in the private bundle and are never
// offered to the receiver.
func ProfileContentManifestFromStaged(source string, sharedPaths []string) (*ProfileContentManifest, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs ProfileContentManifest::from_staged")
}

// sortedObjectKeys is the manifest's object keys in BTreeMap (byte) order.
func (m *ProfileContentManifest) sortedObjectKeys() []string {
	keys := make([]string, 0, len(m.objects))
	for key := range m.objects {
		keys = append(keys, key)
	}
	slices.Sort(keys)
	return keys
}

// privateBundle is the private entries as a gzip tar spool, rewound; nil
// when there are none.
func (m *ProfileContentManifest) privateBundle() (*os.File, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs ProfileContentManifest::private_bundle")
}

// TryTransfer transfers to a fresh staging home, or prewarms just the
// non-secret objects when destination is nil. A false result means the host
// lacks the receiver runtime. No profile bytes have been sent then.
func (m *ProfileContentManifest) TryTransfer(executor core.CommandExecutor, ssh *core.SshTarget, destination *string) (bool, error) {
	untimed := core.NewWithoutDeadline(executor)
	defer untimed.End()
	ok, err := supported(untimed, ssh)
	if err != nil || !ok {
		return false, err
	}
	if err := transfer(untimed, ssh, m, destination); err != nil {
		return false, err
	}
	return true, nil
}

func (m *ProfileContentManifest) batch(negotiation *negotiation, materialize bool) (*os.File, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs ProfileContentManifest::batch")
}

// archive spools what populate writes into a gzip tar, following symlinks
// as the Rust builder does, and returns the spool rewound.
func archive(populate func(archive *tar.Writer) error) (*os.File, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs archive")
}

type negotiation struct {
	claimed []string
	busy    bool
}

// negotiationRead is Rust's `Negotiation::read`: the receiver's reply, one
// line per offered object in order, then an empty line.
func negotiationRead(response *bufio.Reader, manifest *ProfileContentManifest) (negotiation, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs Negotiation::read")
}

// replies reads the receiver's stdout chunks as they arrive. A nil chunk is
// Rust's None: the stream ended.
type replies struct {
	chunks <-chan []byte
	next   *bytes.Reader
}

func (r *replies) Read(buffer []byte) (int, error) {
	if r.next == nil || r.next.Len() == 0 {
		chunk, ok := <-r.chunks
		if !ok || chunk == nil {
			return 0, io.EOF
		}
		r.next = bytes.NewReader(chunk)
	}
	return r.next.Read(buffer)
}

// batchInput is the receiver's stdin: the offer, then, once the receiver
// has answered it, the batch of claimed objects (and, when materializing,
// the manifest and private bundle).
type batchInput struct {
	manifest    *ProfileContentManifest
	materialize bool
	offer       *bytes.Reader
	// replies is taken (set nil) once the negotiation is read.
	replies     *replies
	archive     *os.File
	negotiation *negotiation
	bytesSent   uint64
	// sendStarted is nil until the batch starts sending.
	sendStarted *time.Time
	sendElapsed time.Duration
}

func (b *batchInput) Read(buffer []byte) (int, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs BatchInput::read")
}

// supported is whether the host has the receiver's runtime: false when its
// Perl is absent or lacks a module.
func supported(executor core.CommandExecutor, ssh *core.SshTarget) (bool, error) {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs supported")
}

// transfer sends manifest's content to the host, materializing it at
// destination, or only offering the shareable objects when destination is
// nil.
func transfer(executor core.CommandExecutor, ssh *core.SshTarget, manifest *ProfileContentManifest, destination *string) error {
	panic("unported: mj-worker-staging/src/worker_binary/profile_transfer.rs transfer")
}
