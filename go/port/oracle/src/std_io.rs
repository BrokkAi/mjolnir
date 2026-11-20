//! Oracle module `std_io`: `std::io` and `std::fs`, for the Go packages
//! `go/rs/rsio` and `go/rs/rsfs`.
//!
//! Requests are objects with an `"op"` member. Bytes travel as hexadecimal
//! text, and so do paths, so that names which are not Unicode survive JSON.
//! An `io::Error` is always reported the same way: its kind (the variant name
//! and the description), its raw operating system code, and its `Display`
//! and `Debug` text.
//!
//! Operations for `rsio`:
//!
//! - `os_error {code}`: the error for an operating system code, and the
//!   error that is only that code's kind.
//! - `kind {name}`, `custom {kind, message}`, `custom_os {kind, code}`,
//!   `nested {kind, inner, message}`: the other forms of `io::Error`.
//! - `reader {shape, .., steps}`: a reader of a given shape (a plain reader
//!   that hands out its data in chunks and can fail, wrapped or not in
//!   `BufReader`, `Take`, `Chain`; a `Cursor`; a slice) driven through `Read`
//!   and `BufRead` calls. The reply holds each step's result and the sizes
//!   of the reads the plain readers were asked for.
//! - `writer {shape, .., steps, finish}`: a writer that records what it is
//!   given, and can accept only part or fail, behind nothing, a `BufWriter`
//!   or a `LineWriter`.
//! - `cursor {kind, init, steps}`: reads, writes and seeks on a `Cursor`.
//! - `copy {reader, writer}`: `io::copy` between each pair of shapes.
//!
//! Operation for `rsfs`:
//!
//! - `fs {umask, steps}`: a script of `std::fs` calls run in a fresh
//!   directory, which is the working directory while it runs. The reply
//!   holds each step's value or error.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, BufWriter, Cursor, LineWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use serde_json::{Value, json};

pub fn handle(request: &Value) -> Value {
    match crate::text(request, "op") {
        "os_error" => {
            let code = int(request, "code") as i32;
            let error = io::Error::from_raw_os_error(code);
            json!({
                "error": err_json(&error),
                "kind_only": err_json(&io::Error::from(error.kind())),
            })
        }
        "kind" => match kind_from_name(crate::text(request, "name")) {
            Some(kind) => err_json(&io::Error::from(kind)),
            None => crate::unknown(request),
        },
        "custom" => match kind_from_name(crate::text(request, "kind")) {
            Some(kind) => {
                let error = io::Error::new(kind, crate::text(request, "message"));
                json!({
                    "error": err_json(&error),
                    "inner": error.get_ref().map(|inner| inner.to_string()),
                    "source": std::error::Error::source(&error).map(|source| source.to_string()),
                })
            }
            None => crate::unknown(request),
        },
        "other" => err_json(&io::Error::other(crate::text(request, "message"))),
        "custom_os" => match kind_from_name(crate::text(request, "kind")) {
            Some(kind) => {
                let code = int(request, "code") as i32;
                err_json(&io::Error::new(kind, io::Error::from_raw_os_error(code)))
            }
            None => crate::unknown(request),
        },
        "nested" => {
            let (Some(kind), Some(inner)) = (
                kind_from_name(crate::text(request, "kind")),
                kind_from_name(crate::text(request, "inner")),
            ) else {
                return crate::unknown(request);
            };
            let error = io::Error::new(kind, io::Error::new(inner, crate::text(request, "message")));
            json!({
                "error": err_json(&error),
                "kind_of_inner": err_json(&io::Error::new(kind, io::Error::from(inner))),
            })
        }
        "reader" => reader_op(request),
        "writer" => writer_op(request),
        "cursor" => cursor_op(request),
        "copy" => copy_op(request),
        "fs" => fs_op(request),
        _ => crate::unknown(request),
    }
}

// --- encoding ------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 15) as usize] as char);
    }
    out
}

fn unhex(text: &str) -> Vec<u8> {
    let digit = |byte: u8| match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => panic!("oracle: bad hexadecimal digit"),
    };
    text.as_bytes().chunks(2).map(|pair| digit(pair[0]) << 4 | digit(pair[1])).collect()
}

/// Bytes in a request: hexadecimal text, or `{"lcg": seed, "len": n}` for
/// `n` pseudo-random bytes, or `{"repeat": hex, "times": n}`.
fn decode_bytes(value: Option<&Value>) -> Vec<u8> {
    match value {
        Some(Value::String(text)) => unhex(text),
        Some(Value::Object(spec)) => {
            if let Some(seed) = spec.get("lcg").and_then(Value::as_u64) {
                let len = spec.get("len").and_then(Value::as_u64).unwrap_or_default() as usize;
                let alphabet = spec.get("alphabet").and_then(Value::as_str).map(unhex);
                let mut state = seed;
                let mut out = Vec::with_capacity(len);
                for _ in 0..len {
                    // Knuth's MMIX generator; the high bits are the good ones.
                    state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    let byte = (state >> 56) as u8;
                    out.push(match &alphabet {
                        Some(alphabet) if !alphabet.is_empty() => alphabet[byte as usize % alphabet.len()],
                        _ => byte,
                    });
                }
                out
            } else {
                let unit = unhex(spec.get("repeat").and_then(Value::as_str).unwrap_or_default());
                let times = spec.get("times").and_then(Value::as_u64).unwrap_or_default() as usize;
                unit.repeat(times)
            }
        }
        _ => Vec::new(),
    }
}

fn bytes(value: &Value, key: &str) -> Vec<u8> {
    decode_bytes(value.get(key))
}

/// Bytes in a reply: hexadecimal text when they are few, and otherwise their
/// length and FNV-1a hash, so that large transfers do not swell the corpus.
fn show(bytes: &[u8]) -> String {
    if bytes.len() <= 96 {
        return hex(bytes);
    }
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("len:{}:fnv:{hash:016x}", bytes.len())
}

fn int(value: &Value, key: &str) -> i64 {
    value.get(key).and_then(Value::as_i64).unwrap_or_default()
}

fn uint(value: &Value, key: &str) -> u64 {
    value.get(key).and_then(Value::as_u64).unwrap_or_default()
}

fn size(value: &Value, key: &str) -> usize {
    uint(value, key) as usize
}

fn optional_size(value: &Value, key: &str) -> Option<usize> {
    value.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn flag(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or_default()
}

fn err_json(error: &io::Error) -> Value {
    json!({
        "kind": format!("{:?}", error.kind()),
        "kind_text": error.kind().to_string(),
        "raw": error.raw_os_error(),
        "display": error.to_string(),
        "debug": format!("{error:?}"),
    })
}

fn result_json<T: Into<Value>>(result: io::Result<T>) -> Value {
    match result {
        Ok(value) => json!({ "ok": value.into() }),
        Err(error) => json!({ "err": err_json(&error) }),
    }
}

fn unit_json(result: io::Result<()>) -> Value {
    result_json(result.map(|()| Value::Null))
}

/// The kinds a stable program can name. The others (`FilesystemLoop`,
/// `InProgress`, ..) are reached through `os_error`.
fn kind_from_name(name: &str) -> Option<io::ErrorKind> {
    use io::ErrorKind::*;
    Some(match name {
        "NotFound" => NotFound,
        "PermissionDenied" => PermissionDenied,
        "ConnectionRefused" => ConnectionRefused,
        "ConnectionReset" => ConnectionReset,
        "HostUnreachable" => HostUnreachable,
        "NetworkUnreachable" => NetworkUnreachable,
        "ConnectionAborted" => ConnectionAborted,
        "NotConnected" => NotConnected,
        "AddrInUse" => AddrInUse,
        "AddrNotAvailable" => AddrNotAvailable,
        "NetworkDown" => NetworkDown,
        "BrokenPipe" => BrokenPipe,
        "AlreadyExists" => AlreadyExists,
        "WouldBlock" => WouldBlock,
        "NotADirectory" => NotADirectory,
        "IsADirectory" => IsADirectory,
        "DirectoryNotEmpty" => DirectoryNotEmpty,
        "ReadOnlyFilesystem" => ReadOnlyFilesystem,
        "StaleNetworkFileHandle" => StaleNetworkFileHandle,
        "InvalidInput" => InvalidInput,
        "InvalidData" => InvalidData,
        "TimedOut" => TimedOut,
        "WriteZero" => WriteZero,
        "StorageFull" => StorageFull,
        "NotSeekable" => NotSeekable,
        "QuotaExceeded" => QuotaExceeded,
        "FileTooLarge" => FileTooLarge,
        "ResourceBusy" => ResourceBusy,
        "ExecutableFileBusy" => ExecutableFileBusy,
        "Deadlock" => Deadlock,
        "CrossesDevices" => CrossesDevices,
        "TooManyLinks" => TooManyLinks,
        "InvalidFilename" => InvalidFilename,
        "ArgumentListTooLong" => ArgumentListTooLong,
        "Interrupted" => Interrupted,
        "Unsupported" => Unsupported,
        "UnexpectedEof" => UnexpectedEof,
        "OutOfMemory" => OutOfMemory,
        "Other" => Other,
        _ => return None,
    })
}

// --- readers ---------------------------------------------------------------

type ReadLog = Rc<RefCell<Vec<usize>>>;

/// A reader over fixed data that hands out at most `chunk` bytes a call,
/// can fail once or report an interruption once at a given offset, and
/// records the size of every buffer it is handed.
struct Chunked {
    data: Vec<u8>,
    pos: usize,
    chunk: usize,
    fail_at: Option<usize>,
    interrupt_at: Option<usize>,
    log: ReadLog,
}

impl Chunked {
    fn new(spec: &Value, data_key: &str, chunk_key: &str, log: &ReadLog) -> Chunked {
        Chunked {
            data: bytes(spec, data_key),
            pos: 0,
            chunk: size(spec, chunk_key),
            fail_at: if data_key == "data" { optional_size(spec, "fail_at") } else { None },
            interrupt_at: if data_key == "data" { optional_size(spec, "interrupt_at") } else { None },
            log: Rc::clone(log),
        }
    }
}

impl Read for Chunked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.log.borrow_mut().push(buf.len());
        if self.interrupt_at.is_some_and(|at| self.pos >= at) {
            self.interrupt_at = None;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        if self.fail_at.is_some_and(|at| self.pos >= at) {
            self.fail_at = None;
            return Err(io::Error::other("injected read failure"));
        }
        let mut n = buf.len().min(self.data.len() - self.pos);
        if self.chunk != 0 {
            n = n.min(self.chunk);
        }
        if let Some(at) = self.fail_at {
            n = n.min(at - self.pos);
        }
        if let Some(at) = self.interrupt_at {
            n = n.min(at - self.pos);
        }
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

enum AnyReader {
    Plain(Box<dyn Read>),
    Buffered(Box<dyn BufRead>),
}

fn build_reader(spec: &Value, log: &ReadLog, log2: &ReadLog) -> Option<AnyReader> {
    let first = || Chunked::new(spec, "data", "chunk", log);
    let second = || Chunked::new(spec, "data2", "chunk2", log2);
    let cap = size(spec, "cap");
    let limit = uint(spec, "limit");
    Some(match crate::text(spec, "shape") {
        "plain" => AnyReader::Plain(Box::new(first())),
        "bufreader" => AnyReader::Buffered(Box::new(BufReader::with_capacity(cap, first()))),
        "take" => AnyReader::Plain(Box::new(first().take(limit))),
        "take_bufreader" => {
            AnyReader::Buffered(Box::new(BufReader::with_capacity(cap, first()).take(limit)))
        }
        "bufreader_take" => {
            AnyReader::Buffered(Box::new(BufReader::with_capacity(cap, first().take(limit))))
        }
        "chain" => AnyReader::Plain(Box::new(first().chain(second()))),
        "chain_bufreader" => AnyReader::Buffered(Box::new(
            BufReader::with_capacity(cap, first()).chain(BufReader::with_capacity(cap, second())),
        )),
        "bufreader_chain" => {
            AnyReader::Buffered(Box::new(BufReader::with_capacity(cap, first().chain(second()))))
        }
        "cursor" => AnyReader::Buffered(Box::new(Cursor::new(bytes(spec, "data")))),
        "cursor_chain" => AnyReader::Buffered(Box::new(
            Cursor::new(bytes(spec, "data")).chain(Cursor::new(bytes(spec, "data2"))),
        )),
        "slice" => {
            let data: &'static [u8] = Box::leak(bytes(spec, "data").into_boxed_slice());
            AnyReader::Buffered(Box::new(data))
        }
        "empty" => AnyReader::Buffered(Box::new(io::empty())),
        "repeat" => AnyReader::Plain(Box::new(io::repeat(uint(spec, "byte") as u8))),
        _ => return None,
    })
}

fn step_name(step: &Value) -> &str {
    step.get(0).and_then(Value::as_str).unwrap_or_default()
}

fn step_size(step: &Value, index: usize) -> usize {
    step.get(index).and_then(Value::as_u64).unwrap_or_default() as usize
}

fn step_bytes(step: &Value, index: usize) -> Vec<u8> {
    decode_bytes(step.get(index))
}

fn step_text(step: &Value, index: usize) -> &str {
    step.get(index).and_then(Value::as_str).unwrap_or_default()
}

/// A Vec holding `prefix`, with at least the capacity the step asks for.
fn vec_with(prefix: Vec<u8>, capacity: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(capacity.max(prefix.len()));
    buf.extend_from_slice(&prefix);
    buf
}

fn read_step(reader: &mut dyn Read, step: &Value) -> Option<Value> {
    Some(match step_name(step) {
        "read" => {
            let mut buf = vec![0u8; step_size(step, 1)];
            match reader.read(&mut buf) {
                Ok(n) => json!({ "n": n, "data": show(&buf[..n]) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "read_exact" => {
            let mut buf = vec![0u8; step_size(step, 1)];
            match reader.read_exact(&mut buf) {
                Ok(()) => json!({ "data": show(&buf) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "read_to_end" => {
            let mut buf = vec_with(step_bytes(step, 1), step_size(step, 2));
            match reader.read_to_end(&mut buf) {
                Ok(n) => json!({ "n": n, "data": show(&buf), "cap": buf.capacity() }),
                Err(error) => {
                    json!({ "err": err_json(&error), "data": show(&buf), "cap": buf.capacity() })
                }
            }
        }
        "read_to_string" => {
            let mut text = step_text(step, 1).to_owned();
            match reader.read_to_string(&mut text) {
                Ok(n) => json!({ "n": n, "text": text }),
                Err(error) => json!({ "err": err_json(&error), "text": text }),
            }
        }
        "bytes" => {
            let mut out = Vec::new();
            let mut by_ref = reader;
            for byte in (&mut by_ref).bytes().take(step_size(step, 1)) {
                out.push(match byte {
                    Ok(byte) => json!(byte),
                    Err(error) => json!({ "err": err_json(&error) }),
                });
            }
            Value::Array(out)
        }
        _ => return None,
    })
}

fn bufread_step(reader: &mut dyn BufRead, step: &Value) -> Option<Value> {
    Some(match step_name(step) {
        "fill_buf" => match reader.fill_buf() {
            Ok(buf) => json!({ "data": show(buf) }),
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "consume" => {
            reader.consume(step_size(step, 1));
            Value::Null
        }
        "read_until" => {
            let mut buf = step_bytes(step, 2);
            match reader.read_until(step_size(step, 1) as u8, &mut buf) {
                Ok(n) => json!({ "n": n, "data": show(&buf) }),
                Err(error) => json!({ "err": err_json(&error), "data": show(&buf) }),
            }
        }
        "skip_until" => match reader.skip_until(step_size(step, 1) as u8) {
            Ok(n) => json!({ "n": n }),
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "read_line" => {
            let mut text = step_text(step, 1).to_owned();
            match reader.read_line(&mut text) {
                Ok(n) => json!({ "n": n, "text": text }),
                Err(error) => json!({ "err": err_json(&error), "text": text }),
            }
        }
        "lines" => {
            let mut out = Vec::new();
            let mut by_ref = reader;
            for line in (&mut by_ref).lines().take(step_size(step, 1)) {
                out.push(match line {
                    Ok(line) => json!({ "line": line }),
                    Err(error) => json!({ "err": err_json(&error) }),
                });
            }
            Value::Array(out)
        }
        "split" => {
            let mut out = Vec::new();
            let mut by_ref = reader;
            for part in (&mut by_ref).split(step_size(step, 1) as u8).take(step_size(step, 2)) {
                out.push(match part {
                    Ok(part) => json!({ "part": show(&part) }),
                    Err(error) => json!({ "err": err_json(&error) }),
                });
            }
            Value::Array(out)
        }
        _ => {
            let mut by_ref = reader;
            return read_step(&mut by_ref, step);
        }
    })
}

fn reader_op(request: &Value) -> Value {
    let log: ReadLog = Rc::default();
    let log2: ReadLog = Rc::default();
    let Some(mut reader) = build_reader(request, &log, &log2) else {
        return crate::unknown(request);
    };
    let mut results = Vec::new();
    for step in request.get("steps").and_then(Value::as_array).into_iter().flatten() {
        let result = match &mut reader {
            AnyReader::Plain(reader) => read_step(reader.as_mut(), step),
            AnyReader::Buffered(reader) => bufread_step(reader.as_mut(), step),
        };
        match result {
            Some(result) => results.push(result),
            None => return crate::unknown(step),
        }
    }
    drop(reader);
    json!({ "results": results, "reads": *log.borrow(), "reads2": *log2.borrow() })
}

// --- writers ---------------------------------------------------------------

type WriteLog = Rc<RefCell<Vec<Value>>>;

/// A writer that records every call, accepts at most `max` bytes a call, and
/// can refuse (accept nothing) from some total on or fail once at one.
struct Recorder {
    max: usize,
    zero_at: Option<usize>,
    err_at: Option<usize>,
    total: usize,
    log: WriteLog,
}

impl Recorder {
    fn new(spec: &Value, log: &WriteLog) -> Recorder {
        Recorder {
            max: size(spec, "max"),
            zero_at: optional_size(spec, "zero_at"),
            err_at: optional_size(spec, "err_at"),
            total: 0,
            log: Rc::clone(log),
        }
    }
}

impl Write for Recorder {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.err_at.is_some_and(|at| self.total >= at) {
            self.err_at = None;
            self.log.borrow_mut().push(json!({ "failed": buf.len() }));
            return Err(io::Error::other("injected write failure"));
        }
        if self.zero_at.is_some_and(|at| self.total >= at) {
            self.log.borrow_mut().push(json!({ "refused": buf.len() }));
            return Ok(0);
        }
        let mut n = buf.len();
        if self.max != 0 {
            n = n.min(self.max);
        }
        if let Some(at) = self.err_at {
            n = n.min(at - self.total);
        }
        if let Some(at) = self.zero_at {
            n = n.min(at - self.total);
        }
        self.log.borrow_mut().push(json!({ "w": show(&buf[..n]), "of": buf.len() }));
        self.total += n;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.log.borrow_mut().push(json!("flush"));
        Ok(())
    }
}

fn write_step(writer: &mut dyn Write, step: &Value) -> Option<Value> {
    Some(match step_name(step) {
        "write" => result_json(writer.write(&step_bytes(step, 1))),
        "write_all" => unit_json(writer.write_all(&step_bytes(step, 1))),
        "flush" => unit_json(writer.flush()),
        _ => return None,
    })
}

fn writer_op(request: &Value) -> Value {
    let log: WriteLog = Rc::default();
    let recorder = Recorder::new(request, &log);
    let cap = size(request, "cap");
    let steps: Vec<&Value> =
        request.get("steps").and_then(Value::as_array).into_iter().flatten().collect();
    let finish = crate::text(request, "finish");
    let mut results = Vec::new();

    macro_rules! buffered {
        ($writer:expr) => {{
            let mut writer = $writer;
            for step in &steps {
                let result = match step_name(step) {
                    "buffer" => Some(json!(show(writer.buffer()))),
                    "capacity" => Some(json!(writer.capacity())),
                    _ => write_step(&mut writer, step),
                };
                match result {
                    Some(result) => results.push(result),
                    None => return crate::unknown(step),
                }
            }
            log.borrow_mut().push(json!("finish"));
            if finish == "into_inner" {
                match writer.into_inner() {
                    Ok(inner) => {
                        results.push(json!({ "ok": null }));
                        drop(inner);
                    }
                    Err(error) => {
                        results.push(json!({
                            "err": err_json(error.error()),
                            "display": error.to_string(),
                        }));
                        log.borrow_mut().push(json!("drop"));
                        drop(error);
                    }
                }
            } else {
                drop(writer);
            }
        }};
    }

    match crate::text(request, "shape") {
        "plain" => {
            let mut writer = recorder;
            for step in &steps {
                match write_step(&mut writer, step) {
                    Some(result) => results.push(result),
                    None => return crate::unknown(step),
                }
            }
        }
        "bufwriter" => buffered!(BufWriter::with_capacity(cap, recorder)),
        "linewriter" => {
            let mut writer = LineWriter::with_capacity(cap, recorder);
            for step in &steps {
                match write_step(&mut writer, step) {
                    Some(result) => results.push(result),
                    None => return crate::unknown(step),
                }
            }
            log.borrow_mut().push(json!("finish"));
            if finish == "into_inner" {
                match writer.into_inner() {
                    Ok(inner) => {
                        results.push(json!({ "ok": null }));
                        drop(inner);
                    }
                    Err(error) => {
                        results.push(json!({
                            "err": err_json(error.error()),
                            "display": error.to_string(),
                        }));
                        log.borrow_mut().push(json!("drop"));
                        drop(error);
                    }
                }
            } else {
                drop(writer);
            }
        }
        _ => return crate::unknown(request),
    }
    json!({ "results": results, "log": *log.borrow() })
}

// --- cursor ----------------------------------------------------------------

fn seek_from(step: &Value) -> Option<SeekFrom> {
    let offset = step.get(2)?;
    Some(match step_text(step, 1) {
        "start" => SeekFrom::Start(offset.as_u64()?),
        "end" => SeekFrom::End(offset.as_i64()?),
        "current" => SeekFrom::Current(offset.as_i64()?),
        _ => return None,
    })
}

fn seek_step(seeker: &mut dyn Seek, step: &Value) -> Option<Value> {
    Some(match step_name(step) {
        "seek" => result_json(seeker.seek(seek_from(step)?)),
        "rewind" => unit_json(seeker.rewind()),
        "stream_position" => result_json(seeker.stream_position()),
        "seek_relative" => unit_json(seeker.seek_relative(step.get(1)?.as_i64()?)),
        _ => return None,
    })
}

fn cursor_op(request: &Value) -> Value {
    let init = bytes(request, "init");
    let steps: Vec<&Value> =
        request.get("steps").and_then(Value::as_array).into_iter().flatten().collect();
    let mut results = Vec::new();

    macro_rules! run {
        ($cursor:expr) => {{
            let mut cursor = $cursor;
            for step in &steps {
                let result = match step_name(step) {
                    "position" => Some(json!(cursor.position())),
                    "set_position" => {
                        cursor.set_position(step.get(1).and_then(Value::as_u64).unwrap_or_default());
                        Some(Value::Null)
                    }
                    "get_ref" => Some(json!(show(cursor.get_ref().as_ref()))),
                    _ => write_step(&mut cursor, step)
                        .or_else(|| seek_step(&mut cursor, step))
                        .or_else(|| bufread_step(&mut cursor, step)),
                };
                match result {
                    Some(result) => results.push(result),
                    None => return crate::unknown(step),
                }
            }
            (cursor.position(), show(cursor.get_ref().as_ref()))
        }};
    }

    let (position, content) = match crate::text(request, "kind") {
        "vec" => run!(Cursor::new(init)),
        "slice" => {
            let mut storage = init;
            run!(Cursor::new(&mut storage[..]))
        }
        _ => return crate::unknown(request),
    };
    json!({ "results": results, "position": position, "content": content })
}

// --- copy ------------------------------------------------------------------

fn copy_op(request: &Value) -> Value {
    let reader = &request["reader"];
    let writer = &request["writer"];
    let reads: ReadLog = Rc::default();
    let writes: WriteLog = Rc::default();
    let mut sink: Vec<u8> = vec_with(bytes(writer, "prefix"), size(writer, "vec_cap"));
    let read_cap = size(reader, "cap");
    let write_cap = size(writer, "cap");

    macro_rules! with_writer {
        ($reader:expr) => {{
            let mut source = $reader;
            match crate::text(writer, "shape") {
                "plain" => io::copy(&mut source, &mut Recorder::new(writer, &writes)),
                "bufwriter" => {
                    let mut target = BufWriter::with_capacity(write_cap, Recorder::new(writer, &writes));
                    let result = io::copy(&mut source, &mut target);
                    writes.borrow_mut().push(json!("finish"));
                    drop(target);
                    result
                }
                "vec" => io::copy(&mut source, &mut sink),
                _ => return crate::unknown(request),
            }
        }};
    }

    let result = match crate::text(reader, "shape") {
        "plain" => with_writer!(Chunked::new(reader, "data", "chunk", &reads)),
        "bufreader" => with_writer!(BufReader::with_capacity(
            read_cap,
            Chunked::new(reader, "data", "chunk", &reads)
        )),
        "cursor" => with_writer!(Cursor::new(bytes(reader, "data"))),
        "slice" => {
            let data = bytes(reader, "data");
            with_writer!(&data[..])
        }
        "take" => with_writer!(Chunked::new(reader, "data", "chunk", &reads).take(uint(reader, "limit"))),
        _ => return crate::unknown(request),
    };
    json!({
        "result": result_json(result),
        "reads": *reads.borrow(),
        "writes": *writes.borrow(),
        "vec": show(&sink),
    })
}

// --- fs --------------------------------------------------------------------

#[cfg(unix)]
unsafe extern "C" {
    fn umask(mask: u32) -> u32;
    fn mkfifo(path: *const std::ffi::c_char, mode: u32) -> i32;
    fn geteuid() -> u32;
    fn getegid() -> u32;
}

fn os_string(bytes: Vec<u8>) -> std::ffi::OsString {
    // SAFETY: the Go tests send the encoded bytes of an OsStr: anything on
    // Unix, and only UTF-8 to a Windows build.
    unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(bytes) }
}

/// A path of a step: the hexadecimal bytes of the path, in which the text
/// `$ROOT` at the start stands for the script's directory.
fn step_path(step: &Value, key: &str, root: &Path) -> PathBuf {
    let raw = bytes(step, key);
    if let Some(rest) = raw.strip_prefix(b"$ROOT") {
        let mut full = root.as_os_str().as_encoded_bytes().to_vec();
        full.extend_from_slice(rest);
        return PathBuf::from(os_string(full));
    }
    PathBuf::from(os_string(raw))
}

/// A path in a reply: with the script's directory replaced by `$ROOT`.
fn reply_path(path: &Path, root: &Path) -> Value {
    let raw = path.as_os_str().as_encoded_bytes();
    let root = root.as_os_str().as_encoded_bytes();
    match raw.strip_prefix(root) {
        Some(rest) => {
            let mut out = b"$ROOT".to_vec();
            out.extend_from_slice(rest);
            json!(hex(&out))
        }
        None => json!(hex(raw)),
    }
}

fn file_type_name(file_type: fs::FileType) -> &'static str {
    if file_type.is_file() {
        return "file";
    }
    if file_type.is_dir() {
        return "dir";
    }
    if file_type.is_symlink() {
        return "symlink";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_fifo() {
            return "fifo";
        }
        if file_type.is_socket() {
            return "socket";
        }
        if file_type.is_block_device() {
            return "block";
        }
        if file_type.is_char_device() {
            return "char";
        }
    }
    "other"
}

fn metadata_json(metadata: &fs::Metadata) -> Value {
    let file_type = metadata.file_type();
    let mut out = json!({
        "type": file_type_name(file_type),
        "is_file": metadata.is_file(),
        "is_dir": metadata.is_dir(),
        "is_symlink": metadata.is_symlink(),
        "readonly": metadata.permissions().readonly(),
    });
    // The size of a directory depends on the file system.
    if !metadata.is_dir() {
        out["len"] = json!(metadata.len());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        out["mode"] = json!(metadata.mode());
        out["permissions_mode"] = json!(metadata.permissions().mode());
        out["own_uid"] = json!(metadata.uid() == unsafe { geteuid() });
        out["own_gid"] = json!(metadata.gid() == unsafe { getegid() });
        if !metadata.is_dir() {
            out["nlink"] = json!(metadata.nlink());
            out["size"] = json!(metadata.size());
        }
        out["mtime_matches"] = json!(
            metadata.modified().ok().and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                == Some(std::time::Duration::new(metadata.mtime() as u64, metadata.mtime_nsec() as u32))
                || metadata.mtime() < 0
        );
    }
    out
}

fn time_json(time: io::Result<std::time::SystemTime>) -> Value {
    match time {
        Ok(time) => match time.duration_since(std::time::UNIX_EPOCH) {
            Ok(since) => json!({ "ok": { "secs": since.as_secs(), "nanos": since.subsec_nanos() } }),
            Err(before) => {
                let before = before.duration();
                json!({ "ok": { "before_secs": before.as_secs(), "before_nanos": before.subsec_nanos() } })
            }
        },
        Err(error) => json!({ "err": err_json(&error) }),
    }
}

fn system_time(step: &Value, secs: &str, nanos: &str) -> std::time::SystemTime {
    let secs = int(step, secs);
    let nanos = uint(step, nanos) as u32;
    if secs >= 0 {
        std::time::UNIX_EPOCH + std::time::Duration::new(secs as u64, nanos)
    } else {
        std::time::UNIX_EPOCH - std::time::Duration::new(secs.unsigned_abs(), 0)
            + std::time::Duration::new(0, nanos)
    }
}

fn open_options(step: &Value) -> fs::OpenOptions {
    let mut options = fs::OpenOptions::new();
    options
        .read(flag(step, "read"))
        .write(flag(step, "write"))
        .append(flag(step, "append"))
        .truncate(flag(step, "truncate"))
        .create(flag(step, "create"))
        .create_new(flag(step, "create_new"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(mode) = step.get("mode").and_then(Value::as_u64) {
            options.mode(mode as u32);
        }
        if let Some(flags) = step.get("custom_flags").and_then(Value::as_i64) {
            options.custom_flags(flags as i32);
        }
    }
    options
}

fn try_lock_json(result: Result<(), fs::TryLockError>) -> Value {
    match result {
        Ok(()) => json!({ "ok": null }),
        Err(error) => {
            let display = error.to_string();
            let debug = format!("{error:?}");
            match error {
                fs::TryLockError::WouldBlock => {
                    json!({ "would_block": true, "display": display, "debug": debug })
                }
                fs::TryLockError::Error(error) => {
                    json!({ "err": err_json(&error), "display": display, "debug": debug })
                }
            }
        }
    }
}

struct Script {
    root: PathBuf,
    files: HashMap<String, fs::File>,
    #[cfg(unix)]
    listeners: Vec<std::os::unix::net::UnixListener>,
}

fn fs_op(request: &Value) -> Value {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let created = std::env::temp_dir().join(format!("mj-oracle-std-io-{}-{serial}", std::process::id()));
    fs::create_dir_all(&created).expect("create the script directory");
    let root = fs::canonicalize(&created).expect("canonicalize the script directory");
    let previous_dir = std::env::current_dir().expect("current directory");
    std::env::set_current_dir(&root).expect("enter the script directory");
    #[cfg(unix)]
    let previous_umask = unsafe { umask(uint(request, "umask") as u32) };

    let mut script = Script {
        root: root.clone(),
        files: HashMap::new(),
        #[cfg(unix)]
        listeners: Vec::new(),
    };
    let mut results = Vec::new();
    let mut unknown = None;
    for step in request.get("steps").and_then(Value::as_array).into_iter().flatten() {
        match fs_step(&mut script, step) {
            Some(result) => results.push(result),
            None => {
                unknown = Some(crate::unknown(step));
                break;
            }
        }
    }
    drop(script);

    #[cfg(unix)]
    unsafe {
        umask(previous_umask);
    }
    std::env::set_current_dir(previous_dir).expect("leave the script directory");
    make_removable(&root);
    let _ = fs::remove_dir_all(&root);
    unknown.unwrap_or_else(|| json!({ "results": results }))
}

/// Gives the owner every permission on everything under `path`, so that a
/// script which took permissions away leaves nothing behind.
fn make_removable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(metadata) = fs::symlink_metadata(path) else { return };
        if !metadata.is_dir() {
            return;
        }
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                make_removable(&entry.path());
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn fs_step(script: &mut Script, step: &Value) -> Option<Value> {
    let root = script.root.clone();
    let p = step_path(step, "p", &root);
    let q = step_path(step, "q", &root);
    let name = crate::text(step, "f").to_owned();
    Some(match crate::text(step, "do") {
        "write" => unit_json(fs::write(&p, bytes(step, "data"))),
        "read" => result_json(fs::read(&p).map(|data| hex(&data))),
        "read_to_string" => result_json(fs::read_to_string(&p)),
        "copy" => result_json(fs::copy(&p, &q)),
        "rename" => unit_json(fs::rename(&p, &q)),
        "remove_file" => unit_json(fs::remove_file(&p)),
        "remove_dir" => unit_json(fs::remove_dir(&p)),
        "remove_dir_all" => unit_json(fs::remove_dir_all(&p)),
        "create_dir" => unit_json(fs::create_dir(&p)),
        "create_dir_all" => unit_json(fs::create_dir_all(&p)),
        "hard_link" => unit_json(fs::hard_link(&p, &q)),
        "read_link" => result_json(fs::read_link(&p).map(|target| reply_path(&target, &root))),
        "canonicalize" => result_json(fs::canonicalize(&p).map(|path| reply_path(&path, &root))),
        "path_canonicalize" => result_json(p.canonicalize().map(|path| reply_path(&path, &root))),
        "metadata" => result_json(fs::metadata(&p).map(|metadata| metadata_json(&metadata))),
        "symlink_metadata" => {
            result_json(fs::symlink_metadata(&p).map(|metadata| metadata_json(&metadata)))
        }
        "path_metadata" => result_json(p.metadata().map(|metadata| metadata_json(&metadata))),
        "path_symlink_metadata" => {
            result_json(p.symlink_metadata().map(|metadata| metadata_json(&metadata)))
        }
        "exists" => result_json(fs::exists(&p)),
        "try_exists" => result_json(p.try_exists()),
        "path_exists" => json!(p.exists()),
        "path_is_file" => json!(p.is_file()),
        "path_is_dir" => json!(p.is_dir()),
        "path_is_symlink" => json!(p.is_symlink()),
        "path_read_link" => result_json(p.read_link().map(|target| reply_path(&target, &root))),
        "read_dir" | "path_read_dir" => {
            let listing = if crate::text(step, "do") == "read_dir" { fs::read_dir(&p) } else { p.read_dir() };
            match listing {
                Ok(listing) => {
                    let mut entries = Vec::new();
                    for entry in listing {
                        match entry {
                            Ok(entry) => entries.push((
                                entry.file_name().as_encoded_bytes().to_vec(),
                                json!({
                                    "name": hex(entry.file_name().as_encoded_bytes()),
                                    "path_is_joined": entry.path() == p.join(entry.file_name()),
                                    "type": entry.file_type().map(file_type_name).ok(),
                                    "metadata_type": entry
                                        .metadata()
                                        .map(|metadata| file_type_name(metadata.file_type()))
                                        .ok(),
                                }),
                            )),
                            Err(error) => {
                                return Some(json!({ "entry_err": err_json(&error) }));
                            }
                        }
                    }
                    // The order of a directory depends on the file system.
                    entries.sort_by(|a, b| a.0.cmp(&b.0));
                    json!({ "ok": entries.into_iter().map(|entry| entry.1).collect::<Vec<_>>() })
                }
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "open" => match open_options(step).open(&p) {
            Ok(file) => {
                script.files.insert(name, file);
                json!({ "ok": null })
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "file_open" => match fs::File::open(&p) {
            Ok(file) => {
                script.files.insert(name, file);
                json!({ "ok": null })
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "file_create" => match fs::File::create(&p) {
            Ok(file) => {
                script.files.insert(name, file);
                json!({ "ok": null })
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "file_create_new" => match fs::File::create_new(&p) {
            Ok(file) => {
                script.files.insert(name, file);
                json!({ "ok": null })
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "close" => {
            script.files.remove(&name);
            Value::Null
        }
        "set_times_now_check" => {
            // Unused placeholder kept out of the protocol.
            return None;
        }
        other if other.starts_with("file_") => {
            let Some(file) = script.files.get_mut(&name) else {
                return Some(json!({ "oracle_error": format!("no open file {name}") }));
            };
            return file_step(file, other, step, &mut script_clone_slot(step), &root).map(|(result, clone)| {
                if let Some((clone_name, clone)) = clone {
                    script.files.insert(clone_name, clone);
                }
                result
            });
        }
        other => return unix_step(script, other, step, &p, &q),
    })
}

/// The name a `file_try_clone` step gives the clone.
fn script_clone_slot(step: &Value) -> String {
    crate::text(step, "as").to_owned()
}

type CloneSlot = Option<(String, fs::File)>;

fn file_step(
    file: &mut fs::File,
    action: &str,
    step: &Value,
    clone_name: &mut String,
    _root: &Path,
) -> Option<(Value, CloneSlot)> {
    let value = match action {
        "file_write" => result_json(file.write(&bytes(step, "data"))),
        "file_write_all" => unit_json(file.write_all(&bytes(step, "data"))),
        "file_flush" => unit_json(file.flush()),
        "file_read" => {
            let mut buf = vec![0u8; size(step, "n")];
            match file.read(&mut buf) {
                Ok(n) => json!({ "ok": hex(&buf[..n]) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "file_read_exact" => {
            let mut buf = vec![0u8; size(step, "n")];
            match file.read_exact(&mut buf) {
                Ok(()) => json!({ "ok": hex(&buf) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "file_read_to_end" => {
            let mut buf = bytes(step, "prefix");
            match file.read_to_end(&mut buf) {
                Ok(n) => json!({ "ok": n, "data": hex(&buf) }),
                Err(error) => json!({ "err": err_json(&error), "data": hex(&buf) }),
            }
        }
        "file_read_to_string" => {
            let mut text = crate::text(step, "prefix").to_owned();
            match file.read_to_string(&mut text) {
                Ok(n) => json!({ "ok": n, "text": text }),
                Err(error) => json!({ "err": err_json(&error), "text": text }),
            }
        }
        "file_seek" => {
            let offset = &step["offset"];
            let position = match crate::text(step, "whence") {
                "start" => SeekFrom::Start(offset.as_u64()?),
                "end" => SeekFrom::End(offset.as_i64()?),
                "current" => SeekFrom::Current(offset.as_i64()?),
                _ => return None,
            };
            result_json(file.seek(position))
        }
        "file_stream_position" => result_json(file.stream_position()),
        "file_rewind" => unit_json(file.rewind()),
        "file_set_len" => unit_json(file.set_len(uint(step, "len"))),
        "file_metadata" => result_json(file.metadata().map(|metadata| metadata_json(&metadata))),
        "file_sync_all" => unit_json(file.sync_all()),
        "file_sync_data" => unit_json(file.sync_data()),
        "file_try_clone" => match file.try_clone() {
            Ok(clone) => {
                return Some((json!({ "ok": null }), Some((std::mem::take(clone_name), clone))));
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "file_lock" => unit_json(file.lock()),
        "file_lock_shared" => unit_json(file.lock_shared()),
        "file_try_lock" => try_lock_json(file.try_lock()),
        "file_try_lock_shared" => try_lock_json(file.try_lock_shared()),
        "file_unlock" => unit_json(file.unlock()),
        "file_set_modified" => unit_json(file.set_modified(system_time(step, "secs", "nanos"))),
        "file_set_times" => {
            let mut times = fs::FileTimes::new();
            if step.get("secs").is_some() {
                times = times.set_modified(system_time(step, "secs", "nanos"));
            }
            if step.get("asecs").is_some() {
                times = times.set_accessed(system_time(step, "asecs", "ananos"));
            }
            unit_json(file.set_times(times))
        }
        "file_modified" => time_json(file.metadata().and_then(|metadata| metadata.modified())),
        "file_accessed" => time_json(file.metadata().and_then(|metadata| metadata.accessed())),
        "file_is_terminal" => json!(io::IsTerminal::is_terminal(&*file)),
        other => return file_unix_step(file, other, step),
    };
    Some((value, None))
}

#[cfg(unix)]
fn file_unix_step(file: &mut fs::File, action: &str, step: &Value) -> Option<(Value, CloneSlot)> {
    use std::os::unix::fs::{FileExt, PermissionsExt};
    let value = match action {
        "file_set_permissions" => {
            unit_json(file.set_permissions(fs::Permissions::from_mode(uint(step, "mode") as u32)))
        }
        "file_read_at" => {
            let mut buf = vec![0u8; size(step, "n")];
            match file.read_at(&mut buf, uint(step, "offset")) {
                Ok(n) => json!({ "ok": hex(&buf[..n]) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "file_read_exact_at" => {
            let mut buf = vec![0u8; size(step, "n")];
            match file.read_exact_at(&mut buf, uint(step, "offset")) {
                Ok(()) => json!({ "ok": hex(&buf) }),
                Err(error) => json!({ "err": err_json(&error) }),
            }
        }
        "file_write_at" => result_json(file.write_at(&bytes(step, "data"), uint(step, "offset"))),
        "file_write_all_at" => {
            unit_json(file.write_all_at(&bytes(step, "data"), uint(step, "offset")))
        }
        "file_fchown_self" => {
            use std::os::fd::AsFd;
            unit_json(std::os::unix::fs::fchown(file.as_fd(), Some(unsafe { geteuid() }), Some(unsafe { getegid() })))
        }
        "file_cloexec" => {
            unsafe extern "C" {
                fn fcntl(fd: i32, cmd: i32, ...) -> i32;
            }
            use std::os::fd::AsRawFd;
            // F_GETFD is 1 and FD_CLOEXEC is 1 on every Unix.
            json!(unsafe { fcntl(file.as_raw_fd(), 1) } & 1 == 1)
        }
        _ => return None,
    };
    Some((value, None))
}

#[cfg(not(unix))]
fn file_unix_step(_file: &mut fs::File, _action: &str, _step: &Value) -> Option<(Value, CloneSlot)> {
    None
}

#[cfg(unix)]
fn unix_step(script: &mut Script, action: &str, step: &Value, p: &Path, q: &Path) -> Option<Value> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    Some(match action {
        "symlink" => unit_json(std::os::unix::fs::symlink(p, q)),
        "set_permissions" => {
            unit_json(fs::set_permissions(p, fs::Permissions::from_mode(uint(step, "mode") as u32)))
        }
        "set_readonly" => match fs::metadata(p) {
            Ok(metadata) => {
                let mut permissions = metadata.permissions();
                permissions.set_readonly(flag(step, "readonly"));
                let mode = permissions.mode();
                match fs::set_permissions(p, permissions) {
                    Ok(()) => json!({ "ok": mode }),
                    Err(error) => json!({ "err": err_json(&error) }),
                }
            }
            Err(error) => json!({ "err": err_json(&error) }),
        },
        "dir_builder" => {
            let mut builder = fs::DirBuilder::new();
            builder.recursive(flag(step, "recursive"));
            if let Some(mode) = step.get("mode").and_then(Value::as_u64) {
                builder.mode(mode as u32);
            }
            unit_json(builder.create(p))
        }
        "chown_self" => unit_json(std::os::unix::fs::chown(
            p,
            Some(unsafe { geteuid() }),
            Some(unsafe { getegid() }),
        )),
        "chown_none" => unit_json(std::os::unix::fs::chown(p, None, None)),
        "lchown_self" => unit_json(std::os::unix::fs::lchown(
            p,
            Some(unsafe { geteuid() }),
            Some(unsafe { getegid() }),
        )),
        "chown_root" => unit_json(std::os::unix::fs::chown(p, Some(0), Some(0))),
        "mkfifo" => {
            let path = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
            let status = unsafe { mkfifo(path.as_ptr(), uint(step, "mode") as u32) };
            json!(status == 0)
        }
        "socket" => match std::os::unix::net::UnixListener::bind(p) {
            Ok(listener) => {
                script.listeners.push(listener);
                json!(true)
            }
            Err(_) => json!(false),
        },
        _ => return None,
    })
}

#[cfg(not(unix))]
fn unix_step(_script: &mut Script, _action: &str, _step: &Value, _p: &Path, _q: &Path) -> Option<Value> {
    None
}
