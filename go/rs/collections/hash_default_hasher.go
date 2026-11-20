// Port of core 1.99.1 src/hash/sip.rs (SipHasher13), std 1.99.1
// src/hash/random.rs (DefaultHasher) and the provided methods of the Hasher
// trait in core's src/hash/mod.rs

package collections

import (
	"encoding/binary"
	"math/bits"
)

// HashMapDefaultHasher is Rust's std::collections::hash_map::DefaultHasher
// (std::hash::DefaultHasher): SipHash 1-3 with both keys zero. Its output
// for the same sequence of writes is the same as Rust's on a little-endian
// 64-bit target, which every target of the port is.
//
// The zero value is DefaultHasher::default(), the same as
// HashMapDefaultHasherNew. Finish does not end the hasher: more can be
// written, and Finish called again.
//
// The methods are those of the Hasher trait. A Rust `value.hash(&mut
// hasher)` feeds the hasher through them, type by type: a `str` or `String`
// is WriteStr, an integer is the Write method of its width, a `char` is
// WriteU32, a `bool` is WriteU8, a slice or Vec is WriteLengthPrefix followed
// by its elements, a tuple or a struct with a derived Hash is its fields in
// order, and an enum with a derived Hash is WriteIsize of the variant's
// position followed by its fields. WriteU128 and WriteI128 are not provided.
type HashMapDefaultHasher struct {
	started bool
	length  int // how many bytes we've processed
	// v0, v2 and v1, v3 show up in pairs in the algorithm.
	v0, v2, v1, v3 uint64
	tail           uint64 // unprocessed bytes le
	ntail          int    // how many bytes in tail are valid
}

// HashMapDefaultHasherNew is DefaultHasher::new.
func HashMapDefaultHasherNew() HashMapDefaultHasher {
	var hasher HashMapDefaultHasher
	hasher.start()
	return hasher
}

// HashMapDefaultHasherDefault is DefaultHasher::default.
func HashMapDefaultHasherDefault() HashMapDefaultHasher {
	return HashMapDefaultHasherNew()
}

// start is Hasher::reset with the keys k0 = k1 = 0, done once.
func (h *HashMapDefaultHasher) start() {
	if h.started {
		return
	}
	h.started = true
	h.length = 0
	h.v0 = 0x736f6d6570736575
	h.v1 = 0x646f72616e646f6d
	h.v2 = 0x6c7967656e657261
	h.v3 = 0x7465646279746573
	h.ntail = 0
}

// compress is one SipRound.
func (h *HashMapDefaultHasher) compress() {
	h.v0 += h.v1
	h.v2 += h.v3
	h.v1 = bits.RotateLeft64(h.v1, 13)
	h.v1 ^= h.v0
	h.v3 = bits.RotateLeft64(h.v3, 16)
	h.v3 ^= h.v2
	h.v0 = bits.RotateLeft64(h.v0, 32)

	h.v2 += h.v1
	h.v0 += h.v3
	h.v1 = bits.RotateLeft64(h.v1, 17)
	h.v1 ^= h.v2
	h.v3 = bits.RotateLeft64(h.v3, 21)
	h.v3 ^= h.v0
	h.v2 = bits.RotateLeft64(h.v2, 32)
}

// sipU8to64Le loads an integer of the desired type from a byte stream, in
// LE order: the len bytes (fewer than 8) of buf from start.
func sipU8to64Le(buf []byte, start, length int) uint64 {
	i := 0 // current byte index (from LSB) in the output u64
	var out uint64
	if i+3 < length {
		out = uint64(binary.LittleEndian.Uint32(buf[start+i:]))
		i += 4
	}
	if i+1 < length {
		out |= uint64(binary.LittleEndian.Uint16(buf[start+i:])) << (i * 8)
		i += 2
	}
	if i < length {
		out |= uint64(buf[start+i]) << (i * 8)
	}
	return out
}

// Write is Hasher::write: it writes some data into the hasher.
func (h *HashMapDefaultHasher) Write(msg []byte) {
	h.start()
	length := len(msg)
	h.length += length

	needed := 0

	if h.ntail != 0 {
		needed = 8 - h.ntail
		h.tail |= sipU8to64Le(msg, 0, min(length, needed)) << (8 * h.ntail)
		if length < needed {
			h.ntail += length
			return
		}
		h.v3 ^= h.tail
		h.compress()
		h.v0 ^= h.tail
		h.ntail = 0
	}

	// Buffered tail is now flushed, process new input.
	remaining := length - needed
	left := remaining & 0x7 // len % 8

	i := needed
	for i < remaining-left {
		mi := binary.LittleEndian.Uint64(msg[i:])

		h.v3 ^= mi
		h.compress()
		h.v0 ^= mi

		i += 8
	}

	h.tail = sipU8to64Le(msg, i, left)
	h.ntail = left
}

// WriteStr is Hasher::write_str: it writes a single `str` into the hasher.
// This hasher works byte-wise, and `0xFF` cannot show up in a `str`, so
// just hashing the one extra byte is enough to be prefix-free.
func (h *HashMapDefaultHasher) WriteStr(s string) {
	h.Write([]byte(s))
	h.WriteU8(0xFF)
}

// Finish is Hasher::finish: the hash value for the values written so far.
func (h HashMapDefaultHasher) Finish() uint64 {
	h.start()
	b := (uint64(h.length)&0xff)<<56 | h.tail

	h.v3 ^= b
	h.compress()
	h.v0 ^= b

	h.v2 ^= 0xff
	h.compress()
	h.compress()
	h.compress()

	return h.v0 ^ h.v1 ^ h.v2 ^ h.v3
}

// WriteU8 is Hasher::write_u8.
func (h *HashMapDefaultHasher) WriteU8(i uint8) {
	h.Write([]byte{i})
}

// WriteU16 is Hasher::write_u16.
func (h *HashMapDefaultHasher) WriteU16(i uint16) {
	h.Write(binary.LittleEndian.AppendUint16(nil, i))
}

// WriteU32 is Hasher::write_u32.
func (h *HashMapDefaultHasher) WriteU32(i uint32) {
	h.Write(binary.LittleEndian.AppendUint32(nil, i))
}

// WriteU64 is Hasher::write_u64.
func (h *HashMapDefaultHasher) WriteU64(i uint64) {
	h.Write(binary.LittleEndian.AppendUint64(nil, i))
}

// WriteUsize is Hasher::write_usize.
func (h *HashMapDefaultHasher) WriteUsize(i int) {
	h.WriteU64(uint64(i))
}

// WriteI8 is Hasher::write_i8.
func (h *HashMapDefaultHasher) WriteI8(i int8) {
	h.WriteU8(uint8(i))
}

// WriteI16 is Hasher::write_i16.
func (h *HashMapDefaultHasher) WriteI16(i int16) {
	h.WriteU16(uint16(i))
}

// WriteI32 is Hasher::write_i32.
func (h *HashMapDefaultHasher) WriteI32(i int32) {
	h.WriteU32(uint32(i))
}

// WriteI64 is Hasher::write_i64.
func (h *HashMapDefaultHasher) WriteI64(i int64) {
	h.WriteU64(uint64(i))
}

// WriteIsize is Hasher::write_isize.
func (h *HashMapDefaultHasher) WriteIsize(i int) {
	h.WriteUsize(i)
}

// WriteLengthPrefix is Hasher::write_length_prefix: it writes a length
// prefix into the hasher, as part of being prefix-free.
func (h *HashMapDefaultHasher) WriteLengthPrefix(length int) {
	h.WriteUsize(length)
}
