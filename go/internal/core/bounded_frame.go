// Port of mj-core/src/bounded_frame.rs.
//
// Newline-delimited frames read from a stream under a byte limit.
//
// Every JSON-lines peer (the relay proxy, the worker's socket, and the Codex
// and Grok quota processes) reads frames the same way. What differs is what a
// partial frame at end of stream means to that peer, so that decision stays
// with the caller.

package core

import (
	"bufio"
	"bytes"
	"errors"
	"io"
)

// BoundedFrame is the outcome of reading one frame.
type BoundedFrame interface {
	isBoundedFrame()
}

// BoundedFrameLine is a complete frame, without its newline.
type BoundedFrameLine struct {
	Bytes []byte
}

// BoundedFrameEnd: the stream ended cleanly between frames.
type BoundedFrameEnd struct{}

// BoundedFrameTruncated: the stream ended part way through a frame; these are
// the bytes read.
type BoundedFrameTruncated struct {
	Bytes []byte
}

func (BoundedFrameLine) isBoundedFrame()      {}
func (BoundedFrameEnd) isBoundedFrame()       {}
func (BoundedFrameTruncated) isBoundedFrame() {}

// BoundedFrameError is why a frame could not be read.
type BoundedFrameError interface {
	error
	isBoundedFrameError()
}

// BoundedFrameErrorIo is a read failure from the underlying stream.
type BoundedFrameErrorIo struct {
	Err error
}

func (e BoundedFrameErrorIo) Error() string { return e.Err.Error() }
func (e BoundedFrameErrorIo) Unwrap() error { return e.Err }

// BoundedFrameErrorTooLarge: the frame grew past the limit before its
// newline arrived.
type BoundedFrameErrorTooLarge struct{}

func (BoundedFrameErrorTooLarge) Error() string { return "frame exceeds its byte limit" }

func (BoundedFrameErrorIo) isBoundedFrameError()       {}
func (BoundedFrameErrorTooLarge) isBoundedFrameError() {}

// ReadBoundedFrame reads one frame of at most maximumBytes bytes, excluding
// its newline. The limit is enforced while reading, so an unterminated stream
// never buffers more than the limit.
func ReadBoundedFrame(reader *bufio.Reader, maximumBytes int) (BoundedFrame, error) {
	var frame []byte
	for {
		if _, err := reader.Peek(1); err != nil {
			if !errors.Is(err, io.EOF) {
				return nil, BoundedFrameErrorIo{Err: err}
			}
			if len(frame) == 0 {
				return BoundedFrameEnd{}, nil
			}
			return BoundedFrameTruncated{Bytes: frame}, nil
		}
		available, _ := reader.Peek(reader.Buffered())
		newline := bytes.IndexByte(available, '\n')
		content := len(available)
		if newline >= 0 {
			content = newline
		}
		if len(frame)+content > maximumBytes {
			return nil, BoundedFrameErrorTooLarge{}
		}
		frame = append(frame, available[:content]...)
		if newline >= 0 {
			reader.Discard(content + 1)
			return BoundedFrameLine{Bytes: frame}, nil
		}
		reader.Discard(content)
	}
}
