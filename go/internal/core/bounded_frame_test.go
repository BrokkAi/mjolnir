// Port of the tests in mj-core/src/bounded_frame.rs.

package core

import (
	"bufio"
	"bytes"
	"io"
	"reflect"
	"strings"
	"testing"
)

// Frames larger than a pipe buffer arrive in many reads; each must come back
// whole, the stream must end cleanly after the last one, and a frame one byte
// over the limit must be refused without buffering all of it.
func TestFramesLargerThanAPipeBufferAreReadWholeAndBounded(t *testing.T) {
	const frame = 200 * 1024
	pipeReader, pipeWriter := io.Pipe()
	go func() {
		for _, b := range []byte("ab") {
			pipeWriter.Write(bytes.Repeat([]byte{b}, frame))
			pipeWriter.Write([]byte("\n"))
		}
		pipeWriter.Write(bytes.Repeat([]byte{'c'}, frame+1))
	}()
	defer pipeReader.Close()
	reader := bufio.NewReaderSize(pipeReader, 64*1024)

	for _, b := range []byte("ab") {
		read, err := ReadBoundedFrame(reader, frame)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(read, BoundedFrameLine{Bytes: bytes.Repeat([]byte{b}, frame)}) {
			t.Fatalf("frame %c was not read whole", b)
		}
	}
	if _, err := ReadBoundedFrame(reader, frame); err != (BoundedFrameErrorTooLarge{}) {
		t.Fatalf("expected TooLarge, got %v", err)
	}
}

func TestEndOfStreamIsCleanBetweenFramesAndTruncatedInsideOne(t *testing.T) {
	reader := bufio.NewReader(strings.NewReader("one\npartial"))
	for _, expected := range []BoundedFrame{
		BoundedFrameLine{Bytes: []byte("one")},
		BoundedFrameTruncated{Bytes: []byte("partial")},
		BoundedFrameEnd{},
	} {
		read, err := ReadBoundedFrame(reader, 64)
		if err != nil {
			t.Fatal(err)
		}
		if !reflect.DeepEqual(read, expected) {
			t.Fatalf("read %#v, expected %#v", read, expected)
		}
	}
}
