// Port of mj-worker-staging/src/worker_binary/profile_transfer.rs.

// Receiver-owned negotiation for staged profile files.

package workerstaging

import (
	"archive/tar"
	"bufio"
	"bytes"
	"compress/gzip"
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"slices"
	"strings"
	"sync"
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
	entries, err := profileArchiveEntries(source, sharedPaths)
	if err != nil {
		return nil, err
	}
	manifest := &ProfileContentManifest{
		source:  source,
		entries: []manifestEntry{},
		objects: map[string]profileObject{},
	}
	for _, entry := range entries {
		path := entry.path
		absolute := pathJoin(source, path)
		if !entry.directory && entry.key == nil {
			manifest.private = append(manifest.private, path)
			continue
		}
		metadata, err := os.Stat(absolute)
		if err != nil {
			return nil, rustIOError(err)
		}
		var key *string
		if entry.key != nil {
			// The legacy store's executable suffix belongs to placement,
			// while negotiated content identity is the digest alone.
			stripped := strings.TrimSuffix(*entry.key, ".x")
			if _, ok := manifest.objects[stripped]; !ok {
				manifest.objects[stripped] = profileObject{path: path, size: uint64(metadata.Size())}
			}
			key = &stripped
		}
		// Normalize separators at the wire boundary. A literal backslash in
		// a Unix filename remains part of that filename.
		wirePath := strings.Join(pathComponents(path), "/")
		manifest.entries = append(manifest.entries, manifestEntry{
			Path:      core.LowerHex([]byte(wirePath)),
			Directory: entry.directory,
			Mode:      uint32(metadata.Mode().Perm()),
			Size:      uint64(metadata.Size()),
			Key:       key,
		})
	}
	return manifest, nil
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
	if len(m.private) == 0 {
		return nil, nil
	}
	return archive(func(archive *tar.Writer) error {
		for _, path := range m.private {
			if err := archiveAppendPath(archive, pathJoin(m.source, path), path); err != nil {
				return err
			}
		}
		return nil
	})
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
	var private *os.File
	if materialize {
		bundle, err := m.privateBundle()
		if err != nil {
			return nil, err
		}
		private = bundle
	}
	if private != nil {
		defer private.Close()
	}
	return archive(func(archive *tar.Writer) error {
		for _, key := range negotiation.claimed {
			object := m.objects[key]
			if err := archiveAppendPath(archive, pathJoin(m.source, object.path), "objects/"+key); err != nil {
				return err
			}
		}
		if !materialize {
			return nil
		}
		manifest, err := json.Marshal(m.entries)
		if err != nil {
			return err
		}
		if err := archiveAppendData(archive, "manifest.json", int64(len(manifest)), bytes.NewReader(manifest)); err != nil {
			return err
		}
		if private != nil {
			info, err := private.Stat()
			if err != nil {
				return rustIOError(err)
			}
			if err := archiveAppendData(archive, "private.tar.gz", info.Size(), private); err != nil {
				return err
			}
		}
		return nil
	})
}

// archive spools what populate writes into a gzip tar, following symlinks
// as the Rust builder does, and returns the spool rewound.
func archive(populate func(archive *tar.Writer) error) (*os.File, error) {
	file, err := os.CreateTemp("", ".tmp")
	if err != nil {
		return nil, fmt.Errorf("create private profile transfer spool: %w", rustIOError(err))
	}
	// An anonymous spool, as tempfile::tempfile is: the open file outlives
	// its name.
	os.Remove(file.Name())
	fail := func(err error) (*os.File, error) {
		file.Close()
		return nil, err
	}
	// flate2's Compression::fast().
	compressor, err := gzip.NewWriterLevel(file, gzip.BestSpeed)
	if err != nil {
		return fail(err)
	}
	writer := tar.NewWriter(compressor)
	if err := populate(writer); err != nil {
		return fail(err)
	}
	if err := writer.Close(); err != nil {
		return fail(rustIOError(err))
	}
	if err := compressor.Close(); err != nil {
		return fail(rustIOError(err))
	}
	if _, err := file.Seek(0, io.SeekStart); err != nil {
		return fail(rustIOError(err))
	}
	return file, nil
}

// archiveAppendPath is tar's `Builder::append_path_with_name` with
// follow_symlinks: the file or directory path resolves to, stored as name
// with its metadata.
func archiveAppendPath(archive *tar.Writer, path, name string) error {
	info, err := os.Stat(path)
	if err != nil {
		return rustIOError(err)
	}
	header := &tar.Header{
		Name:    name,
		Mode:    int64(info.Mode().Perm()),
		ModTime: info.ModTime().Truncate(time.Second),
		Format:  tar.FormatGNU,
	}
	if info.IsDir() {
		header.Typeflag = tar.TypeDir
		return rustIOError(archive.WriteHeader(header))
	}
	header.Typeflag = tar.TypeReg
	header.Size = info.Size()
	input, err := os.Open(path)
	if err != nil {
		return rustIOError(err)
	}
	defer input.Close()
	if err := archive.WriteHeader(header); err != nil {
		return rustIOError(err)
	}
	_, err = io.Copy(archive, input)
	return rustIOError(err)
}

// archiveAppendData is tar's `Builder::append_data` with a fresh GNU header
// of mode 0600.
func archiveAppendData(archive *tar.Writer, name string, size int64, data io.Reader) error {
	if err := archive.WriteHeader(&tar.Header{
		Name:     name,
		Mode:     0o600,
		Size:     size,
		Typeflag: tar.TypeReg,
		ModTime:  time.Unix(0, 0),
		Format:   tar.FormatGNU,
	}); err != nil {
		return rustIOError(err)
	}
	_, err := io.Copy(archive, data)
	return rustIOError(err)
}

type negotiation struct {
	claimed []string
	busy    bool
}

// negotiationRead is Rust's `Negotiation::read`: the receiver's reply, one
// line per offered object in order, then an empty line.
func negotiationRead(response *bufio.Reader, manifest *ProfileContentManifest) (negotiation, error) {
	var result negotiation
	readLine := func() (string, error) {
		line, err := response.ReadString('\n')
		if err != nil && !errors.Is(err, io.EOF) {
			return "", err
		}
		return line, nil
	}
	for _, expected := range manifest.sortedObjectKeys() {
		line, err := readLine()
		if err != nil {
			return negotiation{}, err
		}
		text, complete := strings.CutSuffix(line, "\n")
		status, key, split := strings.Cut(text, " ")
		if !complete || !split {
			return negotiation{}, errors.New("invalid profile negotiation reply")
		}
		if key != expected {
			return negotiation{}, fmt.Errorf("profile receiver replied with unexpected object %s", core.RustDebugString(key))
		}
		switch status {
		case "claimed":
			result.claimed = append(result.claimed, key)
		case "present":
		case "busy":
			result.busy = true
		default:
			return negotiation{}, fmt.Errorf("invalid profile negotiation state %s", core.RustDebugString(status))
		}
	}
	line, err := readLine()
	if err != nil {
		return negotiation{}, err
	}
	if line != "\n" {
		return negotiation{}, errors.New("incomplete profile negotiation reply")
	}
	return result, nil
}

// replyQueue is Rust's unbounded mpsc channel of the receiver's stdout
// chunks: pushing never blocks the executor that reports them. A nil chunk
// is Rust's None: the stream ended.
type replyQueue struct {
	mu     sync.Mutex
	ready  sync.Cond
	chunks [][]byte
	ended  bool
}

func newReplyQueue() *replyQueue {
	queue := &replyQueue{}
	queue.ready.L = &queue.mu
	return queue
}

// push queues a copy of chunk; nil ends the stream.
func (q *replyQueue) push(chunk []byte) {
	q.mu.Lock()
	defer q.mu.Unlock()
	if chunk == nil {
		q.ended = true
	} else {
		q.chunks = append(q.chunks, slices.Clone(chunk))
	}
	q.ready.Broadcast()
}

// pop waits for the next chunk; false once the stream ended.
func (q *replyQueue) pop() ([]byte, bool) {
	q.mu.Lock()
	defer q.mu.Unlock()
	for len(q.chunks) == 0 && !q.ended {
		q.ready.Wait()
	}
	if len(q.chunks) == 0 {
		return nil, false
	}
	chunk := q.chunks[0]
	q.chunks = q.chunks[1:]
	return chunk, true
}

// replies reads the receiver's stdout chunks as they arrive.
type replies struct {
	queue *replyQueue
	next  *bytes.Reader
}

func (r *replies) Read(buffer []byte) (int, error) {
	if r.next == nil || r.next.Len() == 0 {
		chunk, ok := r.queue.pop()
		if !ok {
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
	if count, _ := b.offer.Read(buffer); count > 0 {
		return count, nil
	}
	if b.replies != nil {
		replies := b.replies
		b.replies = nil
		negotiation, err := negotiationRead(bufio.NewReader(replies), b.manifest)
		if err != nil {
			return 0, err
		}
		materialize := b.materialize && !negotiation.busy
		if materialize || len(negotiation.claimed) != 0 {
			archive, err := b.manifest.batch(&negotiation, materialize)
			if err != nil {
				return 0, err
			}
			b.archive = archive
			started := time.Now()
			b.sendStarted = &started
		}
		b.negotiation = &negotiation
	}
	if b.archive == nil {
		return 0, io.EOF
	}
	count, err := b.archive.Read(buffer)
	b.bytesSent += uint64(count)
	if count == 0 && errors.Is(err, io.EOF) {
		b.sendElapsed = time.Since(*b.sendStarted)
	}
	return count, err
}

// close releases the batch spool.
func (b *batchInput) close() {
	if b.archive != nil {
		b.archive.Close()
	}
}

// supported is whether the host has the receiver's runtime: false when its
// Perl is absent or lacks a module.
func supported(executor core.CommandExecutor, ssh *core.SshTarget) (bool, error) {
	command := core.SSHCommand(core.SshTrafficClassControl, ssh, []string{"sh", "-c", profileReceiverProbe}).
		WithPurpose("check negotiated profile receiver support")
	output, err := executor.Execute(&command)
	if err != nil {
		return false, err
	}
	switch output.Status {
	case 0:
		return true, nil
	case 2, 127: // Perl compilation failure or Perl absent.
		return false, nil
	default:
		_, err := core.CheckedCommandOutput(&command, output)
		if err == nil {
			panic("a failed receiver probe passed its check")
		}
		return false, err
	}
}

// transfer sends manifest's content to the host, materializing it at
// destination, or only offering the shareable objects when destination is
// nil.
func transfer(executor core.CommandExecutor, ssh *core.SshTarget, manifest *ProfileContentManifest, destination *string) error {
	var offered uint64
	var offer strings.Builder
	for _, key := range manifest.sortedObjectKeys() {
		size := manifest.objects[key].size
		offered += size
		fmt.Fprintf(&offer, "%s %d\n", key, size)
	}
	offer.WriteByte('\n')
	var bytesSent uint64
	objectsSent := 0
	var claimWait, sendTime time.Duration
	var reused int
	for {
		queue := newReplyQueue()
		input := &batchInput{
			manifest:    manifest,
			materialize: destination != nil,
			offer:       bytes.NewReader([]byte(offer.String())),
			replies:     &replies{queue: queue},
		}
		args := []string{"perl", "-e", profileReceiver, profileContentStore}
		if destination != nil {
			args = append(args, *destination)
		}
		command := core.SSHCommand(core.SshTrafficClassBulk, ssh, args).
			WithPurpose("send negotiated profile content").
			WithWarnOnStderr().
			WithStage(core.ProvisionStage{Kind: core.ProvisionStageSyncing}).
			// Input can finish before the child's stdout closes.
			WithStreamReply(queue.push)
		output, err := executor.ExecuteWithStdin(&command, input)
		// Nothing reads the replies once the command has returned.
		queue.push(nil)
		input.close()
		if err != nil {
			return err
		}
		if _, err := core.CheckedCommandOutput(&command, output); err != nil {
			return err
		}
		if input.negotiation == nil {
			return errors.New("missing profile negotiation")
		}
		bytesSent += input.bytesSent
		objectsSent += len(input.negotiation.claimed)
		sendTime += input.sendElapsed
		if !input.negotiation.busy {
			reused = max(len(manifest.objects)-objectsSent, 0)
			break
		}
		start := time.Now()
		queued := core.NewProvisionStageGuard(executor, core.ProvisionStage{Kind: core.ProvisionStageWaitingForWorkerStore})
		waited := core.SleepUnlessCancelled(250*time.Millisecond, executor.CancellationRequested)
		queued.End()
		if !waited {
			return errors.New("operation cancelled while waiting for profile content claims")
		}
		claimWait += time.Since(start)
	}
	attributes := []any{
		"bytes_offered", offered, "bytes_sent", bytesSent, "objects_reused", reused,
		"objects_sent", objectsSent, "claim_wait_ms", uint64(claimWait.Milliseconds()),
		"send_ms", uint64(sendTime.Milliseconds()), "target", ssh.Destination,
	}
	if destination != nil {
		attributes = append(attributes, "destination", *destination)
	}
	slog.Info("send profile content", attributes...)
	return nil
}
