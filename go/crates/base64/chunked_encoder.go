// Port of base64 0.22.1 src/chunked_encoder.rs

package base64

// The output mechanism for ChunkedEncoder's encoded bytes.
type chunkedEncoderSink interface {
	// Handle a chunk of encoded base64 data (as UTF-8 bytes)
	writeEncodedBytes(encoded []byte) error
}

// A base64 encoder that emits encoded bytes in chunks without heap allocation.
type chunkedEncoderChunkedEncoder struct {
	engine Engine
}

func chunkedEncoderChunkedEncoderNew(engine Engine) chunkedEncoderChunkedEncoder {
	return chunkedEncoderChunkedEncoder{engine: engine}
}

func (self chunkedEncoderChunkedEncoder) encode(bytes []byte, sink chunkedEncoderSink) error {
	const bufSize = 1024
	const chunkSize = bufSize / 4 * 3

	var buf [bufSize]byte
	for start := 0; start < len(bytes); start += chunkSize {
		chunk := bytes[start:min(start+chunkSize, len(bytes))]
		len_ := self.engine.InternalEncode(chunk, buf[:])
		if len(chunk) != chunkSize && self.engine.Config().EncodePadding() {
			// Final, potentially partial, chunk.
			// Only need to consider if padding is needed on a partial chunk since full chunk
			// is a multiple of 3, which therefore won't be padded.
			// Pad output to multiple of four bytes if required by config.
			len_ += encodeAddPadding(len_, buf[len_:])
		}
		if err := sink.writeEncodedBytes(buf[:len_]); err != nil {
			return err
		}
	}

	return nil
}

// A really simple sink that just appends to a string
type chunkedEncoderStringSink struct {
	string_ *string
}

func chunkedEncoderStringSinkNew(s *string) *chunkedEncoderStringSink {
	return &chunkedEncoderStringSink{string_: s}
}

func (self *chunkedEncoderStringSink) writeEncodedBytes(s []byte) error {
	// The encoded bytes are ASCII, so they are valid UTF-8.
	*self.string_ += string(s)

	return nil
}
