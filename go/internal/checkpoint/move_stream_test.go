// Port of the tests in mj-checkpoint/src/move_stream.rs.

package checkpoint

import (
	"archive/tar"
	"bytes"
	"crypto/sha256"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/BrokkAi/mjolnir/go/internal/core"
)

func moveStreamSHA(data []byte) string {
	sum := sha256.Sum256(data)
	return core.LowerHex(sum[:])
}

func moveStreamPath(path string) *string { return &path }

func TestStreamsOver128KiBThroughRealPipeWithSkipAndSHA256(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	dir := t.TempDir()
	srcArchive := filepath.Join(dir, "src.hel.zip")
	dstArchive := filepath.Join(dir, "dst.hel.zip")
	srcKeep := filepath.Join(dir, "src_keep")
	dstKeep := filepath.Join(dir, "dst_keep")

	// >128 KiB archive payload to exercise >64 KiB pipe buffers.
	archivePayload := make([]byte, 192*1024)
	for i := range archivePayload {
		archivePayload[i] = byte(i % 251)
	}
	if err := os.WriteFile(srcArchive, archivePayload, 0o644); err != nil {
		t.Fatal(err)
	}
	expectedSHA := moveStreamSHA(archivePayload)

	// >128 KiB keep payload plus a skipped file.
	if err := os.MkdirAll(filepath.Join(srcKeep, "nested"), 0o755); err != nil {
		t.Fatal(err)
	}
	keepBig := make([]byte, 140*1024)
	for i := range keepBig {
		keepBig[i] = byte((i * 7) % 253)
	}
	if err := os.WriteFile(filepath.Join(srcKeep, "nested/big.bin"), keepBig, 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(srcKeep, "skip-me.txt"), []byte("do not send"), 0o644); err != nil {
		t.Fatal(err)
	}

	readEnd, writeEnd, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	type exported struct {
		report MoveStreamExportReport
		err    error
	}
	done := make(chan exported, 1)
	go func() {
		report, err := WriteMoveStream(writeEnd, srcArchive, &srcKeep, []string{"skip-me.txt"})
		writeEnd.Close()
		done <- exported{report, err}
	}()

	receipt, err := ReceiveMoveStream(readEnd, dstArchive, expectedSHA, &dstKeep)
	readEnd.Close()
	if err != nil {
		t.Fatalf("receive_move_stream: %v", err)
	}
	result := <-done
	if result.err != nil {
		t.Fatalf("write_move_stream: %v", result.err)
	}
	export := result.report

	if export.ArchiveBytes != uint64(len(archivePayload)) {
		t.Fatalf("archive bytes %d", export.ArchiveBytes)
	}
	if receipt.ArchiveBytes != export.ArchiveBytes {
		t.Fatalf("receipt archive bytes %d", receipt.ArchiveBytes)
	}
	if receipt.ArchiveSHA256 != expectedSHA {
		t.Fatalf("receipt sha %s", receipt.ArchiveSHA256)
	}
	if receipt.KeepTarBytes != export.KeepTarBytes {
		t.Fatalf("keep tar bytes %d != %d", receipt.KeepTarBytes, export.KeepTarBytes)
	}
	if receipt.KeepTarBytes <= 128*1024 {
		t.Fatalf("keep tar bytes %d", receipt.KeepTarBytes)
	}

	gotArchive, err := os.ReadFile(dstArchive)
	if err != nil || !bytes.Equal(gotArchive, archivePayload) {
		t.Fatalf("dst archive differs: %v", err)
	}
	gotKeep, err := os.ReadFile(filepath.Join(dstKeep, "nested/big.bin"))
	if err != nil || !bytes.Equal(gotKeep, keepBig) {
		t.Fatalf("keep big differs: %v", err)
	}
	if checkpointExists(filepath.Join(dstKeep, "skip-me.txt")) {
		t.Fatal("skipped path must not be extracted")
	}
}

func TestRejectsSHA256MismatchAndTruncatedStream(t *testing.T) {
	dir := t.TempDir()
	srcArchive := filepath.Join(dir, "src.hel.zip")
	dstArchive := filepath.Join(dir, "dst.hel.zip")
	if err := os.WriteFile(srcArchive, []byte("archive-content"), 0o644); err != nil {
		t.Fatal(err)
	}

	var framed bytes.Buffer
	report, err := WriteMoveStream(&framed, srcArchive, nil, nil)
	if err != nil {
		t.Fatalf("write framed: %v", err)
	}

	// 1. SHA-256 mismatch must fail and leave dst_archive absent.
	badSHA := strings.Repeat("0", 64)
	_, err = ReceiveMoveStream(bytes.NewReader(framed.Bytes()), dstArchive, badSHA, nil)
	if err == nil || !strings.Contains(err.Error(), "SHA-256 mismatch") {
		t.Fatalf("expected sha mismatch, got %v", err)
	}
	if checkpointExists(dstArchive) {
		t.Fatal("dst archive exists after sha mismatch")
	}

	// 2. Truncated stream must fail and leave dst_archive absent.
	truncated := framed.Bytes()[:framed.Len()-5]
	_, err = ReceiveMoveStream(bytes.NewReader(truncated), dstArchive, report.ArchiveSHA256, nil)
	if err == nil {
		t.Fatal("expected truncated stream error")
	}
	if !strings.Contains(err.Error(), "truncated") && !strings.Contains(err.Error(), "read") {
		t.Fatalf("unexpected error: %v", err)
	}
	if checkpointExists(dstArchive) {
		t.Fatal("dst archive exists after truncated stream")
	}
}

func TestRejectsSymlinkEscapeInKeepTarExtraction(t *testing.T) {
	dir := t.TempDir()
	badTar := filepath.Join(dir, "escape.tar")
	dstKeep := filepath.Join(dir, "dst_keep")

	// Build a tar containing a symlink entry.
	file, err := os.Create(badTar)
	if err != nil {
		t.Fatal(err)
	}
	builder := tar.NewWriter(file)
	if err := builder.WriteHeader(&tar.Header{
		Typeflag: tar.TypeSymlink,
		Name:     "evil-link",
		Linkname: "/etc/passwd",
		Format:   tar.FormatGNU,
	}); err != nil {
		t.Fatalf("append symlink: %v", err)
	}
	if err := builder.Close(); err != nil {
		t.Fatalf("finish bad tar: %v", err)
	}
	file.Close()

	err = ExtractKeepTarStrict(badTar, dstKeep)
	if err == nil || !strings.Contains(err.Error(), "refusing non-file/non-directory") {
		t.Fatalf("symlink entry must be rejected, got %v", err)
	}
}

func TestRejectsDanglingSymlinkAtExtractionTarget(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	dir := t.TempDir()
	outsideDir := t.TempDir()
	outsideTarget := filepath.Join(outsideDir, "pwned.txt")

	srcKeep := filepath.Join(dir, "src_keep")
	if err := os.MkdirAll(srcKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(srcKeep, "secret.txt"), []byte("sensitive"), 0o644); err != nil {
		t.Fatal(err)
	}

	tarPath := filepath.Join(dir, "keep.tar")
	tarFile, err := os.Create(tarPath)
	if err != nil {
		t.Fatal(err)
	}
	keep, err := core.NewMoveKeep(srcKeep, nil)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := buildKeepTar(keep, tarFile); err != nil {
		t.Fatalf("build tar: %v", err)
	}
	tarFile.Close()

	dstKeep := filepath.Join(dir, "dst_keep")
	if err := os.MkdirAll(dstKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outsideTarget, filepath.Join(dstKeep, "secret.txt")); err != nil {
		t.Fatal(err)
	}

	err = ExtractKeepTarStrict(tarPath, dstKeep)
	if err == nil || !strings.Contains(err.Error(), "refusing symlink") {
		t.Fatalf("dangling symlink at extraction target must be rejected, got %v", err)
	}
	if checkpointExists(outsideTarget) {
		t.Fatal("outside target must not be created through dangling symlink")
	}
}

func TestPreservesExecutableAndPrivatePermissionsOnRoundTrip(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	dir := t.TempDir()
	srcArchive := filepath.Join(dir, "src.hel.zip")
	dstArchive := filepath.Join(dir, "dst.hel.zip")
	srcKeep := filepath.Join(dir, "src_keep")
	dstKeep := filepath.Join(dir, "dst_keep")

	if err := os.WriteFile(srcArchive, []byte("archive-bytes"), 0o644); err != nil {
		t.Fatal(err)
	}
	expectedSHA := moveStreamSHA([]byte("archive-bytes"))

	if err := os.MkdirAll(srcKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	script := filepath.Join(srcKeep, "run.sh")
	secret := filepath.Join(srcKeep, "secret.key")
	if err := os.WriteFile(script, []byte("#!/bin/sh\necho ok\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(secret, []byte("top-secret"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(script, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(secret, 0o600); err != nil {
		t.Fatal(err)
	}

	var framed bytes.Buffer
	if _, err := WriteMoveStream(&framed, srcArchive, &srcKeep, nil); err != nil {
		t.Fatalf("write stream: %v", err)
	}
	if _, err := ReceiveMoveStream(bytes.NewReader(framed.Bytes()), dstArchive, expectedSHA, &dstKeep); err != nil {
		t.Fatalf("receive stream: %v", err)
	}

	scriptInfo, err := os.Stat(filepath.Join(dstKeep, "run.sh"))
	if err != nil {
		t.Fatal(err)
	}
	secretInfo, err := os.Stat(filepath.Join(dstKeep, "secret.key"))
	if err != nil {
		t.Fatal(err)
	}
	if mode := scriptInfo.Mode().Perm(); mode != 0o755 {
		t.Fatalf("executable mode must be preserved, got %o", mode)
	}
	if mode := secretInfo.Mode().Perm(); mode != 0o600 {
		t.Fatalf("private 0600 mode must be preserved, got %o", mode)
	}
}

func TestFailedReceiveLeavesExistingKeepDestIntact(t *testing.T) {
	dir := t.TempDir()
	srcArchive := filepath.Join(dir, "src.hel.zip")
	dstArchive := filepath.Join(dir, "dst.hel.zip")
	srcKeep := filepath.Join(dir, "src_keep")
	dstKeep := filepath.Join(dir, "dst_keep")

	if err := os.WriteFile(srcArchive, []byte("archive-bytes"), 0o644); err != nil {
		t.Fatal(err)
	}
	expectedSHA := moveStreamSHA([]byte("archive-bytes"))
	if err := os.MkdirAll(srcKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(srcKeep, "new.txt"), []byte("incoming"), 0o644); err != nil {
		t.Fatal(err)
	}

	// Pre-populate dst_keep with an existing file that must survive a failed
	// receive.
	if err := os.MkdirAll(dstKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dstKeep, "existing.txt"), []byte("original"), 0o644); err != nil {
		t.Fatal(err)
	}

	var framed bytes.Buffer
	if _, err := WriteMoveStream(&framed, srcArchive, &srcKeep, nil); err != nil {
		t.Fatalf("write framed: %v", err)
	}
	// Append unexpected trailing garbage so the receive fails at EOF check.
	framed.WriteString("EXTRA")

	_, err := ReceiveMoveStream(bytes.NewReader(framed.Bytes()), dstArchive, expectedSHA, &dstKeep)
	if err == nil || !strings.Contains(err.Error(), "unexpected trailing bytes") {
		t.Fatalf("trailing bytes must fail receive, got %v", err)
	}
	if checkpointExists(dstArchive) {
		t.Fatal("archive must not be persisted on failure")
	}
	existing, err := os.ReadFile(filepath.Join(dstKeep, "existing.txt"))
	if err != nil || string(existing) != "original" {
		t.Fatalf("existing keep file must remain: %q %v", existing, err)
	}
	if checkpointExists(filepath.Join(dstKeep, "new.txt")) {
		t.Fatal("staged keep files must not leak into keep_dest on failure")
	}
}

func TestFailedArchivePersistRollsBackKeepDest(t *testing.T) {
	dir := t.TempDir()
	srcArchive := filepath.Join(dir, "src.hel.zip")
	// Make dst_archive an existing directory so persisting the archive
	// fails.
	dstArchive := filepath.Join(dir, "dst_is_dir.hel.zip")
	if err := os.MkdirAll(dstArchive, 0o755); err != nil {
		t.Fatal(err)
	}

	srcKeep := filepath.Join(dir, "src_keep")
	dstKeep := filepath.Join(dir, "dst_keep")
	if err := os.WriteFile(srcArchive, []byte("archive-bytes"), 0o644); err != nil {
		t.Fatal(err)
	}
	expectedSHA := moveStreamSHA([]byte("archive-bytes"))
	if err := os.MkdirAll(srcKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(srcKeep, "new.txt"), []byte("incoming"), 0o644); err != nil {
		t.Fatal(err)
	}

	if err := os.MkdirAll(dstKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(dstKeep, "existing.txt"), []byte("original"), 0o644); err != nil {
		t.Fatal(err)
	}

	var framed bytes.Buffer
	if _, err := WriteMoveStream(&framed, srcArchive, &srcKeep, nil); err != nil {
		t.Fatalf("write framed: %v", err)
	}

	_, err := ReceiveMoveStream(bytes.NewReader(framed.Bytes()), dstArchive, expectedSHA, &dstKeep)
	if err == nil || !strings.Contains(err.Error(), "persist verified archive") {
		t.Fatalf("persisting archive onto a directory must fail, got %v", err)
	}
	existing, err := os.ReadFile(filepath.Join(dstKeep, "existing.txt"))
	if err != nil || string(existing) != "original" {
		t.Fatalf("existing keep must be rolled back: %q %v", existing, err)
	}
	if checkpointExists(filepath.Join(dstKeep, "new.txt")) {
		t.Fatal("new keep file must be rolled back on archive persist failure")
	}
}

func TestRejectsSymlinkedKeepAncestorsOnExportAndReceive(t *testing.T) {
	if runtime.GOOS == "windows" {
		t.Skip("unix only")
	}
	dir := t.TempDir()
	outside := filepath.Join(dir, "outside")
	if err := os.MkdirAll(outside, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(outside, "secret.txt"), []byte("outside-data"), 0o644); err != nil {
		t.Fatal(err)
	}

	srcArchive := filepath.Join(dir, "src.hel.zip")
	if err := os.WriteFile(srcArchive, []byte("archive-bytes"), 0o644); err != nil {
		t.Fatal(err)
	}
	expectedSHA := moveStreamSHA([]byte("archive-bytes"))

	// 1. Export must reject a symlinked `.mj/keep` ancestor.
	badSrcCheckout := filepath.Join(dir, "bad-src")
	if err := os.MkdirAll(filepath.Join(badSrcCheckout, ".mj"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(badSrcCheckout, ".mj/keep")); err != nil {
		t.Fatal(err)
	}
	var out bytes.Buffer
	_, err := WriteMoveStream(&out, srcArchive, moveStreamPath(filepath.Join(badSrcCheckout, ".mj/keep/sess-1")), nil)
	if err == nil || !strings.Contains(err.Error(), "refusing symlink") {
		t.Fatalf("export through symlinked .mj/keep must fail, got %v", err)
	}

	// 2. Export must reject a symlinked `.mj` ancestor.
	badMJCheckout := filepath.Join(dir, "bad-mj")
	if err := os.MkdirAll(badMJCheckout, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(badMJCheckout, ".mj")); err != nil {
		t.Fatal(err)
	}
	_, err = WriteMoveStream(&bytes.Buffer{}, srcArchive, moveStreamPath(filepath.Join(badMJCheckout, ".mj/keep/sess-1")), nil)
	if err == nil || !strings.Contains(err.Error(), "refusing symlink") {
		t.Fatalf("export through symlinked .mj must fail, got %v", err)
	}

	// Build a valid framed stream to test receive rejection.
	goodKeep := filepath.Join(dir, "good-keep")
	if err := os.MkdirAll(goodKeep, 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(goodKeep, "note.txt"), []byte("hello"), 0o644); err != nil {
		t.Fatal(err)
	}
	var framed bytes.Buffer
	if _, err := WriteMoveStream(&framed, srcArchive, &goodKeep, nil); err != nil {
		t.Fatalf("write valid: %v", err)
	}

	// 3. Receive must reject a symlinked `.mj/keep` ancestor and leave
	// `outside` untouched.
	badDstCheckout := filepath.Join(dir, "bad-dst")
	if err := os.MkdirAll(filepath.Join(badDstCheckout, ".mj"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(outside, filepath.Join(badDstCheckout, ".mj/keep")); err != nil {
		t.Fatal(err)
	}
	dstArchive := filepath.Join(dir, "dst.hel.zip")
	_, err = ReceiveMoveStream(bytes.NewReader(framed.Bytes()), dstArchive, expectedSHA, moveStreamPath(filepath.Join(badDstCheckout, ".mj/keep/sess-1")))
	if err == nil || !strings.Contains(err.Error(), "refusing symlink") {
		t.Fatalf("receive through symlinked .mj/keep must fail, got %v", err)
	}
	if checkpointExists(filepath.Join(outside, "sess-1")) {
		t.Fatal("outside/sess-1 must not exist")
	}
	if checkpointExists(dstArchive) {
		t.Fatal("dst archive must not exist")
	}
}
