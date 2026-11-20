"""Writes the Go declarations for a Rust file: every type with its tags and
registration, every constant, and every function and method signature with a
body that panics with rs.Todo.

    python3 go/port/portmap.py declarations mj-core/src/runtime.rs > go/mjcore/runtime.go

Declarations are the mechanical part of a port (field names, serde attributes,
variant lists, receiver kinds), so they are generated instead of typed. What
the generator cannot express is marked `PORT:` in a comment with the Rust text,
for the person porting the file to settle. Bodies and tests are theirs.
"""

from __future__ import annotations

import re
import subprocess

import portmap
from portmap import pascal

PRIMITIVES = {
    "bool": "bool", "u8": "uint8", "u16": "uint16", "u32": "uint32", "u64": "uint64",
    "i8": "int8", "i16": "int16", "i32": "int32", "i64": "int64", "usize": "int", "isize": "int",
    "f32": "float32", "f64": "float64", "char": "rune", "str": "string", "String": "string",
    "u128": "rs.U128", "i128": "rs.I128",
}

# Names every Rust file has without importing them.
PRELUDE = {
    "Option": "std::option::Option", "Vec": "std::vec::Vec", "Box": "std::boxed::Box",
    "Result": "std::result::Result", "String": "std::string::String",
}

# Fully resolved Rust type -> Go type. `%s` receives the translated type
# arguments, comma separated.
KNOWN = {
    "std::string::String": "string", "alloc::string::String": "string",
    "std::path::PathBuf": "string", "std::path::Path": "string",
    "std::ffi::OsString": "string", "std::ffi::OsStr": "string",
    "std::time::Duration": "time.Duration", "core::time::Duration": "time.Duration",
    "std::time::Instant": "rstime.Instant", "std::time::SystemTime": "time.Time",
    "std::collections::HashMap": "collections.HashMap[%s]",
    "std::collections::BTreeMap": "collections.BTreeMap[%s]",
    "std::collections::HashSet": "collections.HashSet[%s]",
    "std::collections::BTreeSet": "collections.BTreeSet[%s]",
    "std::collections::VecDeque": "collections.VecDeque[%s]",
    "std::sync::Mutex": "rssync.Mutex[%s]", "std::sync::RwLock": "rssync.RwLock[%s]",
    "std::sync::Condvar": "rssync.Condvar", "std::sync::OnceLock": "rssync.OnceLock[%s]",
    "std::sync::LazyLock": "rssync.LazyLock[%s]", "std::sync::Once": "rssync.Once",
    "std::sync::Barrier": "rssync.Barrier", "std::sync::Weak": "rs.Weak[%s]",
    "std::cell::RefCell": "rs.RefCell[%s]", "std::cell::Cell": "rs.Cell[%s]",
    "std::io::Error": "error", "std::fmt::Error": "error", "std::fmt::Result": "error",
    "std::fmt::Formatter": "*rs.Formatter",
    "std::fs::File": "*rsfs.File", "std::process::Child": "*rsprocess.Child",
    "std::process::Command": "*rsprocess.Command", "std::process::ExitStatus": "rsprocess.ExitStatus",
    "std::process::Output": "rsprocess.Output", "std::process::Stdio": "rsprocess.Stdio",
    "std::thread::JoinHandle": "*rsthread.JoinHandle[%s]",
    "std::marker::PhantomData": "rs.Unit", "std::convert::Infallible": "rs.Unit",
    "std::cmp::Ordering": "int",
    "std::ops::Range": "rs.Range[%s]", "std::ops::RangeInclusive": "rs.RangeInclusive[%s]",
    "std::num::NonZeroU32": "uint32", "std::num::NonZeroU64": "uint64",
    "std::num::NonZeroUsize": "int", "std::num::NonZeroU8": "uint8", "std::num::NonZeroU16": "uint16",
    "serde_json::Value": "serdejson.Value", "serde_json::Map": "serdejson.Map",
    "serde_json::Number": "serdejson.Number", "serde_json::Error": "error",
    "serde_json::value::RawValue": "serdejson.RawValue",
    "anyhow::Error": "error", "bytes::Bytes": "[]byte", "bytes::BytesMut": "[]byte",
    "chrono::DateTime": "chrono.DateTime", "chrono::Duration": "chrono.TimeDelta",
    "chrono::TimeDelta": "chrono.TimeDelta",
    "tokio::task::JoinHandle": "*tokio.TaskJoinHandle[%s]",
    "tokio::task::JoinSet": "*tokio.TaskJoinSet[%s]",
    "tokio::runtime::Runtime": "*tokio.Runtime", "tokio::runtime::Handle": "tokio.RuntimeHandle",
    "tokio::time::Instant": "tokio.TimeInstant", "tokio::time::Interval": "*tokio.TimeInterval",
    "tokio::sync::Notify": "*tokio.SyncNotify", "tokio::sync::Semaphore": "*tokio.SyncSemaphore",
    "tokio::sync::Mutex": "*tokio.SyncMutex[%s]", "tokio::sync::RwLock": "*tokio.SyncRwLock[%s]",
    "tokio::sync::OnceCell": "*tokio.SyncOnceCell[%s]",
    "tokio_util::sync::CancellationToken": "tokioutil.SyncCancellationToken",
}
for width in ("Bool", "U8", "U16", "U32", "U64", "I8", "I16", "I32", "I64", "Usize", "Isize"):
    KNOWN[f"std::sync::atomic::Atomic{width}"] = f"rssync.Atomic{width}"
KNOWN["std::sync::atomic::AtomicPtr"] = "rssync.AtomicPtr[%s]"

# Types whose only job is to hold another type, by Rust path.
WRAPPERS_SHARED = {"std::sync::Arc", "std::rc::Rc"}
WRAPPERS_OWNED = {"std::boxed::Box"}
TRANSPARENT = {"std::borrow::Cow", "std::pin::Pin", "std::mem::ManuallyDrop", "std::num::Wrapping"}

STD_PACKAGES = {
    "std::fs": "rsfs", "std::io": "rsio", "std::path": "rspath", "std::process": "rsprocess",
    "std::sync::atomic": "rssync", "std::sync::mpsc": "rssync", "std::sync": "rssync",
    "std::thread": "rsthread", "std::time": "rstime", "std::net": "rsnet", "std::env": "rsenv",
    "std::collections": "collections", "std::os::unix::net": "rsnet", "std::os::unix::fs": "rsfs",
    "std::os::fd": "rsfs", "std::os::unix::io": "rsfs", "std::os::unix::process": "rsprocess",
}

IMPORTS = {
    "rs": "github.com/BrokkAi/mjolnir/go/rs",
    "assets": "github.com/BrokkAi/mjolnir/go/assets",
    "math": "math",
    "strs": "github.com/BrokkAi/mjolnir/go/rs/strs",
    "collections": "github.com/BrokkAi/mjolnir/go/rs/collections",
    "context": "context", "time": "time",
}
for _name in ("rsfs", "rsio", "rspath", "rsprocess", "rssync", "rsthread", "rstime", "rsnet", "rsenv"):
    IMPORTS[_name] = f"github.com/BrokkAi/mjolnir/go/rs/{_name}"

GO_PREDECLARED = portmap.GO_KEYWORDS | {
    "len", "cap", "new", "make", "append", "copy", "delete", "panic", "recover", "print", "println",
    "string", "int", "bool", "error", "any", "nil", "true", "false", "iota", "min", "max", "clear",
    "close", "complex", "real", "imag", "byte", "rune", "uint", "float32", "float64",
}

STD_TRAIT_METHODS = {
    ("Display", "fmt"): "FmtDisplay", ("Debug", "fmt"): "FmtDebug", ("Drop", "drop"): "Drop",
    ("Clone", "clone"): "Clone", ("PartialEq", "eq"): "Equal", ("Ord", "cmp"): "Compare",
    ("PartialOrd", "partial_cmp"): "PartialCompare", ("Hash", "hash"): "HashKey",
    ("Iterator", "next"): "Next", ("Deref", "deref"): "Deref", ("DerefMut", "deref_mut"): "DerefMut",
    ("Serialize", "serialize"): "SerializeSerde", ("Deserialize", "deserialize"): "DeserializeSerde",
    ("Error", "source"): "Unwrap",
}


def go_string(text: str) -> str:
    """A Go string literal for text."""
    out = ['"']
    for char in text:
        code = ord(char)
        if char == "\\":
            out.append("\\\\")
        elif char == '"':
            out.append('\\"')
        elif char == "\n":
            out.append("\\n")
        elif char == "\t":
            out.append("\\t")
        elif char == "\r":
            out.append("\\r")
        elif code < 0x20 or code == 0x7F:
            out.append(f"\\x{code:02x}")
        elif code in (0x2028, 0x2029, 0xFEFF):
            out.append(f"\\u{code:04x}")
        else:
            out.append(char)
    out.append('"')
    return "".join(out)


CFG_OS = {
    "macos": "darwin", "linux": "linux", "windows": "windows", "freebsd": "freebsd", "ios": "ios",
    "android": "android", "netbsd": "netbsd", "openbsd": "openbsd", "dragonfly": "dragonfly",
}
CFG_ARCH = {"x86_64": "amd64", "aarch64": "arm64", "x86": "386", "arm": "arm"}


def cfg_constraint(text: str) -> str | None:
    """A Go build constraint for one Rust `cfg` predicate, or None when Go
    has no build-time equivalent."""
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][A-Za-z0-9_]*|[(),=]', text)
    position = 0

    def parse():
        nonlocal position
        if position >= len(tokens):
            return None
        token = tokens[position]
        position += 1
        if token in ("any", "all", "not") and position < len(tokens) and tokens[position] == "(":
            position += 1
            parts = []
            while position < len(tokens) and tokens[position] != ")":
                part = parse()
                if part is None:
                    return None
                parts.append(part)
                if position < len(tokens) and tokens[position] == ",":
                    position += 1
            position += 1
            parts = list(dict.fromkeys(parts))
            if token == "not":
                if not parts:
                    return None
                inner = parts[0]
                if " " in inner and not (inner.startswith("(") and inner.endswith(")")):
                    inner = f"({inner})"
                return "!" + inner
            if not parts:
                return None
            if len(parts) == 1:
                return parts[0]
            joiner = " || " if token == "any" else " && "
            return "(" + joiner.join(parts) + ")"
        if position < len(tokens) and tokens[position] == "=":
            value = tokens[position + 1].strip('"')
            position += 2
            if token == "target_os":
                return CFG_OS.get(value)
            if token == "target_arch":
                return CFG_ARCH.get(value)
            if token == "target_family":
                return value if value in ("unix", "windows") else None
            if token == "feature":
                return "mj_" + value.replace("-", "_")
            return None
        if token in ("unix", "windows"):
            return token
        if token == "test":
            # `any(test, feature = "test-support")`: the tests are built with the tag.
            return "mj_test_support"
        if token == "debug_assertions":
            return "mj_debug"
        return None

    result = parse()
    if result and result.startswith("(") and result.endswith(")"):
        result = result[1:-1]
    return result


def cfg_suffix(constraint: str) -> str:
    """The file name suffix for a build constraint."""
    if re.fullmatch(r"mj_[a-z_]+", constraint) and constraint != "mj_debug":
        return "_feat_" + constraint[3:]
    text = constraint.replace("!", "non").replace("&&", "_and_").replace("||", "_or_")
    text = re.sub(r"[^A-Za-z0-9_]", "", text)
    text = re.sub(r"_+", "_", text)
    suffix = "_cfg_" + text
    last = suffix.split("_")[-1]
    if last in portmap.GO_FILE_SUFFIXES and last != constraint:
        # Go reads a trailing `_darwin` in a file name as a constraint of its own.
        suffix += "_cfg"
    return suffix


def camel(name: str) -> str:
    text = pascal(name.lstrip("_")) if name.strip("_") else "underscore"
    text = text[:1].lower() + text[1:]
    return text + "_" if text in GO_PREDECLARED else text


def lower_first(name: str) -> str:
    return name[:1].lower() + name[1:]


def split_attrs(text: str) -> list[str]:
    """Splits `a = "x, y", b, c(d, e)` at top-level commas."""
    parts, depth, quote, current = [], 0, False, ""
    index = 0
    while index < len(text):
        char = text[index]
        if quote:
            current += char
            if char == "\\":
                index += 1
                current += text[index]
            elif char == '"':
                quote = False
        elif char == '"':
            quote = True
            current += char
        elif char in "([{":
            depth += 1
            current += char
        elif char in ")]}":
            depth -= 1
            current += char
        elif char == "," and depth == 0:
            parts.append(current.strip())
            current = ""
        else:
            current += char
        index += 1
    if current.strip():
        parts.append(current.strip())
    return parts


class Generator:
    def __init__(self, ws: portmap.Workspace, path: str):
        self.ws = ws
        self.crate = next(c for c in ws.crates.values() if any(f["path"] == path for f in c.files))
        self.file = next(f for f in self.crate.files if f["path"] == path)
        self.path = path
        self.used = set()
        self.notes = 0
        self.generic_scope = [set()]
        self.constable_memo = {}
        self.substitution_scope = []
        self.self_type = None
        self.aliases = {}
        self.globs = []
        for use in self.file["uses"]:
            parts = use["path"].split("::")
            if use["glob"]:
                self.globs.append(parts)
            else:
                self.aliases[use["alias"] or parts[-1]] = parts

    # --- resolution -------------------------------------------------------

    @staticmethod
    def _find_item(file, name):
        fallback = None
        for item in file["items"]:
            if item["name"] == name and item["kind"] in ("struct", "enum", "trait", "type", "union"):
                if not item["test_only"]:
                    return item
                fallback = fallback or item
        return fallback

    def _module_file(self, crate, parts):
        """The file holding module `parts`, and the path left over."""
        for length in range(len(parts), -1, -1):
            module = "::".join(parts[:length])
            file = crate.modules.get(module) or crate.inline.get(module)
            if file is not None:
                return file, parts[length:]
        return None, parts

    def _lookup_name(self, crate, file, segments, visited):
        """Resolves a name as it would be seen from inside `file`'s module."""
        key = (crate.dir, file["path"], tuple(segments))
        if key in visited or len(visited) > 200:
            return None
        visited.add(key)
        head = segments[0]
        if len(segments) == 1:
            item = self._find_item(file, head)
            if item:
                return ("item", crate, file, item)
        globs = []
        for use in file["uses"]:
            parts = use["path"].split("::")
            if use["glob"]:
                globs.append(parts)
            elif (use["alias"] or parts[-1]) == head and parts != segments:
                hit = self._lookup_path(crate, file, parts + segments[1:], visited)
                if hit:
                    return hit
        hit = self._lookup_path(crate, file, segments, visited)
        if hit:
            return hit
        for parts in globs:
            hit = self._lookup_path(crate, file, parts + segments, visited)
            # A glob over a workspace module also brings in what that module
            # imported from outside. A glob over an outside crate
            # (`rayon::prelude::*`) cannot be checked, so it proves nothing.
            if hit and (hit[0] == "item" or self._in_workspace(crate, file, parts)):
                return hit
        return None

    def _in_workspace(self, crate, file, parts) -> bool:
        head = parts[0] if parts else ""
        if head in ("crate", "self", "super"):
            return True
        if head in self.ws.by_lib:
            return True
        name = crate.deps.get(head) or crate.dev_deps.get(head)
        if name in self.ws.by_package:
            return True
        module = [p for p in file["module"].split("::") if p]
        child = "::".join(module + [head])
        return child in crate.modules or child in crate.inline

    def _lookup_path(self, crate, file, segments, visited):
        """Resolves a path written in `file`: `crate::a::B`, `super::B`, `tokio::x::Y`, `child::B`."""
        segments = [s for s in segments if s]
        if not segments:
            return None
        head = segments[0]
        own = crate if file["target"] in ("lib", "bin") else None
        module = [p for p in file["module"].split("::") if p] if own else []
        if head == "crate":
            return self._lookup_module(crate, segments[1:], visited) if own else None
        if head == "self":
            return self._lookup_module(crate, module + segments[1:], visited)
        if head == "super":
            parent = list(module)
            rest = list(segments)
            while rest and rest[0] == "super":
                parent = parent[:-1]
                rest = rest[1:]
            return self._lookup_module(crate, parent + rest, visited)
        ws = self.ws
        if head in ws.by_lib and (head in crate.deps or head in crate.dev_deps or ws.by_lib[head] is crate):
            return self._lookup_module(ws.by_lib[head], segments[1:], visited)
        if head in crate.deps or head in crate.dev_deps:
            name = crate.deps.get(head) or crate.dev_deps.get(head)
            if name in ws.by_package:
                return self._lookup_module(ws.by_package[name], segments[1:], visited)
            return ("extern", "::".join(segments))
        if head in ("std", "core", "alloc"):
            return ("extern", "::".join(segments))
        if own and len(segments) > 1:
            child = "::".join(module + [head])
            if child in crate.modules or child in crate.inline:
                return self._lookup_module(crate, module + segments, visited)
        return None

    def _lookup_module(self, crate, parts, visited):
        file, left = self._module_file(crate, parts)
        if file is None or not left:
            return None
        return self._lookup_name(crate, file, left, visited)

    def resolve(self, path_text: str, file=None, crate=None, depth: int = 0):
        """`("item", crate, file, item)`, `("extern", "a::b::C")`, `("generic", name)`,
        `("prim", name)`, `("self", go name)` or None."""
        file = file or self.file
        crate = crate or self.crate
        segments = [s for s in path_text.split("::") if s]
        if not segments:
            return None
        head = segments[0]
        if len(segments) == 1:
            if any(head in scope for scope in self.generic_scope):
                return ("generic", head)
            if head == "Self" and self.self_type:
                return self.self_type
        hit = self._lookup_name(crate, file, segments, set())
        if hit:
            return hit
        if len(segments) == 1:
            if head in PRIMITIVES:
                return ("prim", head)
            if head in PRELUDE:
                return ("extern", PRELUDE[head])
        return None

    def go_item_name(self, crate, file, item) -> str:
        name = self.ws.go_name(crate, file, item)
        if crate is not self.crate:
            self.used.add(crate.go_package)
            return f"{crate.go_package}.{name}"
        return name

    @staticmethod
    def item_shape(item) -> str:
        """struct | copy | newtype | enum (carries data) | cenum | trait | alias."""
        kind = item["kind"]
        if kind == "trait":
            return "trait"
        if kind == "type":
            return "alias"
        decl = item.get("decl") or {}
        if kind == "enum":
            variants = decl.get("variants", [])
            if all(v["kind"] == "unit" for v in variants):
                return "cenum"
            return "enum"
        if decl.get("fields_kind") == "tuple" and len(decl.get("fields", [])) == 1 and not decl["generics"]["params"]:
            inner = decl["fields"][0]["ty"]
            text = inner.get("text", "")
            # A Go defined type over a pointer, interface or function cannot
            # have methods, so those newtypes stay structs with a field F0.
            if inner.get("k") == "path" and inner["path"].split("::")[-1] in (
                "String", "PathBuf", "u8", "u16", "u32", "u64", "i8", "i16", "i32", "i64", "usize",
                "isize", "f32", "f64", "bool", "char", "Vec", "Duration", "OsString",
            ) and not text.startswith(("Arc", "Box", "Rc")):
                return "newtype"
            return "struct"
        if "Copy" in item.get("derives", []):
            return "copy"
        return "struct"

    # --- types ------------------------------------------------------------

    def note(self, rust: str) -> str:
        self.notes += 1
        return f"any /* PORT: {rust.replace('*/', '* /')} */"

    def args_text(self, node, count=None) -> str:
        args = [self.go_type(arg) for arg in node.get("args", []) if arg.get("k") != "const"]
        if count is not None:
            args = args[:count]
        return ", ".join(args)

    def future_output(self, node):
        """The `Output` of a future type, or None."""
        for bound in node.get("bounds", []):
            if bound["path"].split("::")[-1] == "Future":
                for name, value in bound["assoc"]:
                    if name == "Output":
                        return value
        return None

    def fn_bound(self, node):
        for bound in node.get("bounds", []):
            if bound["path"].split("::")[-1] in ("Fn", "FnMut", "FnOnce") and bound.get("paren"):
                return bound["paren"]
        return None

    def func_type(self, inputs, output) -> str:
        params = ", ".join(self.go_type(t, "param") for t in inputs)
        result = self.result_types(output)
        if len(result) == 0:
            return f"func({params})"
        if len(result) == 1:
            return f"func({params}) {result[0]}"
        return f"func({params}) ({', '.join(result)})"

    def bounds_type(self, node, position) -> str:
        output = self.future_output(node)
        if output is not None:
            self.used.add("tokio")
            return f"tokio.Fn[{self.go_type(output, 'generic')}]"
        paren = self.fn_bound(node)
        if paren is not None:
            return self.func_type(paren["inputs"], paren["output"])
        for bound in node.get("bounds", []):
            last = bound["path"].split("::")[-1]
            if last in ("Send", "Sync", "Unpin", "Sized", "Copy", "Clone", "Debug", "UnwindSafe", "RefUnwindSafe"):
                continue
            if last == "Any":
                return "any"
            if last == "Error":
                return "error"
            if last in ("Into", "AsRef", "Borrow", "From") and bound["args"]:
                return self.go_type(bound["args"][0], position)
            if last in ("ToString", "Display"):
                return "any"
            if last in ("IntoIterator", "Iterator", "DoubleEndedIterator", "ExactSizeIterator"):
                for name, value in bound["assoc"]:
                    if name == "Item":
                        return f"[]{self.go_type(value, 'generic')}"
                return self.note(node["text"])
            if last in ("AsyncRead", "AsyncBufRead", "AsyncWrite", "AsyncSeek"):
                names = {b["path"].split("::")[-1] for b in node.get("bounds", [])}
                self.used.add("tokio")
                if "AsyncRead" in names and "AsyncWrite" in names:
                    return "tokio.IoAsyncReadWrite"
                return "tokio.Io" + last
            if last == "BufRead":
                self.used.add("rsio")
                return "rsio.BufRead"
            if last == "Read":
                self.used.add("io")
                return "io.Reader"
            if last == "Write":
                self.used.add("io")
                return "io.Writer"
            if last in ("Serialize", "DeserializeOwned", "Deserialize"):
                return "any"
            hit = self.resolve(bound["path"])
            if hit and hit[0] == "item":
                return self.go_item_name(hit[1], hit[2], hit[3])
            return self.note(node["text"])
        return "any"

    def go_type(self, node, position: str = "field") -> str:
        """position: field | param | generic."""
        if node is None:
            return "rs.Unit"
        kind = node.get("k")
        if kind == "path":
            return self.path_type(node, position)
        if kind == "ref":
            return self.ref_type(node, position)
        if kind == "slice":
            inner = self.go_type(node["t"], "generic")
            return "[]byte" if inner == "uint8" else f"[]{inner}"
        if kind == "array":
            length = node["len"]
            if not re.fullmatch(r"\d+", length):
                hit = self.const_name(length)
                length = hit or f"/* PORT: {length} */ 0"
            inner = self.go_type(node["t"], "generic")
            return f"[{length}]{'byte' if inner == 'uint8' else inner}"
        if kind == "tuple":
            elems = node["elems"]
            if not elems:
                self.used.add("rs")
                return "rs.Unit"
            if len(elems) > 5:
                return self.note(node["text"])
            self.used.add("rs")
            return f"rs.Tuple{len(elems)}[{', '.join(self.go_type(e, 'generic') for e in elems)}]"
        if kind == "fn":
            return self.func_type(node["inputs"], node["output"])
        if kind in ("dyn", "impl"):
            return self.bounds_type(node, position)
        if kind == "never":
            self.used.add("rs")
            return "rs.Unit"
        if kind == "ptr":
            return "*" + self.go_type(node["t"], "generic")
        return self.note(node.get("text", "?"))

    def const_name(self, text: str):
        hit = None
        for item in self.file["items"]:
            if item["kind"] in ("const", "static") and item["name"] == text.split("::")[-1]:
                hit = self.ws.go_name(self.crate, self.file, item)
        return hit

    def path_type(self, node, position) -> str:
        path = node["path"]
        if "::" not in path:
            replaced = self.substituted(path)
            if replaced is not None:
                return replaced
        elif path.endswith("::Output") and path.count("::") == 1:
            owner = path.split("::")[0]
            for scope in reversed(self.substitution_scope):
                if owner in scope and scope[owner][0] == "future":
                    value = scope[owner][1]
                    return self.go_type(value, "generic") if value is not None else owner + "Output"
        hit = self.resolve(path)
        if hit is None:
            last = path.split("::")[-1]
            if last in PRIMITIVES and "::" not in path:
                return self.prim(last)
            return self.note(node["text"])
        if hit[0] == "prim":
            return self.prim(hit[1])
        if hit[0] == "generic":
            return hit[1]
        if hit[0] == "self":
            return hit[1]
        if hit[0] == "item":
            _, crate, file, item = hit
            name = self.go_item_name(crate, file, item)
            if node.get("args") and item.get("decl", {}).get("generics", {}).get("params"):
                args = self.args_text(node)
                if args:
                    name += f"[{args}]"
            return name
        rust = hit[1]
        return self.extern_type(rust, node, position)

    def prim(self, name: str) -> str:
        go = PRIMITIVES[name]
        if go.startswith("rs."):
            self.used.add("rs")
        return go

    def extern_type(self, rust: str, node, position) -> str:
        rust = rust.replace("alloc::", "std::").replace("core::", "std::")
        last = rust.split("::")[-1]
        args = node.get("args", [])
        if rust in ("std::option::Option",):
            self.used.add("rs")
            return f"rs.Option[{self.go_type(args[0], 'generic') if args else 'any'}]"
        if rust in ("std::result::Result", "anyhow::Result", "std::io::Result", "std::fmt::Result", "serde_json::Result"):
            if rust == "std::fmt::Result":
                return "error"
            self.used.add("rs")
            if args and args[0].get("k") == "tuple" and not args[0]["elems"]:
                return "rs.Result[rs.Unit]"
            return f"rs.Result[{self.go_type(args[0], 'generic') if args else 'rs.Unit'}]"
        if rust == "std::vec::Vec":
            inner = self.go_type(args[0], "generic") if args else "any"
            return "[]byte" if inner == "uint8" else f"[]{inner}"
        if rust in WRAPPERS_SHARED or rust in WRAPPERS_OWNED:
            return self.boxed(args[0] if args else None, position)
        if rust in TRANSPARENT:
            return self.go_type(args[0], position) if args else self.note(node["text"])
        if rust.endswith("::BoxFuture") or rust.endswith("::LocalBoxFuture"):
            self.used.add("tokio")
            return f"tokio.Fn[{self.go_type(args[0], 'generic') if args else 'rs.Unit'}]"
        if rust.startswith("agent_client_protocol"):
            self.notes += 1
            self.used.add("acp")
            return f"acp.{last} /* PORT: {rust}; see acp-mapping.md */"
        if rust in KNOWN:
            go = KNOWN[rust]
            if "%s" in go:
                translated = self.args_text(node)
                if rust == "chrono::DateTime":
                    translated = ""
                go = go % translated if translated else go.replace("[%s]", "")
            self.mark(go)
            return go
        if rust.startswith("libc::") or rust.startswith("std::ffi::c_") or rust.startswith("std::os::raw::"):
            scalar = {
                "c_int": "int32", "c_uint": "uint32", "c_long": "int64", "c_ulong": "uint64", "c_char": "int8",
                "c_uchar": "uint8", "c_short": "int16", "c_ushort": "uint16", "c_longlong": "int64",
                "c_ulonglong": "uint64", "pid_t": "int32", "uid_t": "uint32", "gid_t": "uint32",
                "mode_t": "uint32", "off_t": "int64", "size_t": "uint", "ssize_t": "int", "rlim_t": "uint64",
                "time_t": "int64", "dev_t": "uint64", "ino_t": "uint64", "nfds_t": "uint64", "socklen_t": "uint32",
                "c_void": "byte", "c_double": "float64", "c_float": "float32",
            }.get(last)
            if scalar:
                return scalar
        crate_name = rust.split("::")[0]
        if crate_name == "std":
            module = "::".join(rust.split("::")[:-1])
            package = None
            for prefix in sorted(STD_PACKAGES, key=len, reverse=True):
                if module == prefix or module.startswith(prefix + "::"):
                    package = STD_PACKAGES[prefix]
                    break
            if package is None:
                return self.note(node["text"])
            prefix_text = "Mpsc" if module.startswith("std::sync::mpsc") else ""
            self.used.add(package)
            go = f"{package}.{prefix_text}{last}"
        else:
            package = crate_name.replace("_", "")
            self.used.add(package)
            go = f"{package}.{''.join(pascal(s) for s in rust.split('::')[1:-1])}{last}"
        translated = self.args_text(node)
        return f"{go}[{translated}]" if translated else go

    def mark(self, go: str) -> None:
        go = re.sub(r"/\*.*?\*/", "", go)
        for package in re.findall(r"\b([a-z][a-z0-9]*)\.", go):
            self.used.add(package)

    def boxed(self, inner, position) -> str:
        """`Box<T>` and `Arc<T>`: a pointer, unless T already is a reference in Go."""
        if inner is None:
            return self.note("Box<?>")
        if inner.get("k") in ("dyn", "impl", "fn"):
            return self.go_type(inner, position)
        if inner.get("k") == "slice":
            return self.go_type(inner, position)
        text = self.go_type(inner, "generic")
        if inner.get("k") == "path":
            hit = self.resolve(inner["path"])
            if hit and hit[0] == "item" and self.item_shape(hit[3]) in ("enum", "trait"):
                return text
            if hit and hit[0] == "prim" and hit[1] == "str":
                return "string"
            if hit and hit[0] == "extern" and hit[1].split("::")[-1] in ("Path", "OsStr"):
                return "string"
        if text.startswith(("*", "[]", "func(", "tokio.Fn[")) or text in ("string", "error", "any"):
            return text
        return "*" + text

    def ref_type(self, node, position) -> str:
        inner = node["t"]
        text = self.go_type(inner, "generic")
        if inner.get("k") in ("dyn", "impl", "slice", "fn"):
            return "*" + text if node["mut"] and inner.get("k") == "slice" and False else text
        if inner.get("k") == "path":
            hit = self.resolve(inner["path"])
            if hit and hit[0] == "item":
                shape = self.item_shape(hit[3])
                if shape in ("enum", "trait"):
                    return "*" + text if node["mut"] else text
                if shape in ("copy", "cenum", "newtype", "alias"):
                    return "*" + text if node["mut"] else text
                return "*" + text
            if hit and hit[0] == "generic":
                return "*" + text if node["mut"] else text
        if node["mut"]:
            return text if text.startswith("*") else "*" + text
        if text.startswith(("collections.", "rssync.", "rs.RefCell", "rs.Cell")) and not text.startswith("rssync.Atomic"):
            return "*" + text
        if text.startswith("rssync.Atomic"):
            return "*" + text
        return text

    def result_types(self, output) -> list[str]:
        """The Go results of a function returning `output`."""
        if output is None:
            return []
        if output.get("k") == "tuple":
            return [self.go_type(e, "generic") for e in output["elems"]]
        if output.get("k") == "never":
            return []
        if output.get("k") == "path":
            hit = self.resolve(output["path"])
            rust = hit[1] if hit and hit[0] == "extern" else ""
            rust = rust.replace("core::", "std::")
            if rust == "std::fmt::Result":
                return ["error"]
            if rust in ("std::result::Result", "anyhow::Result", "std::io::Result", "serde_json::Result"):
                args = output.get("args", [])
                inner = self.result_types(args[0]) if args else []
                return inner + ["error"]
        return [self.go_type(output, "generic")]

    # --- emission ---------------------------------------------------------

    def docs(self, lines, indent="") -> list[str]:
        out = []
        for line in lines:
            for part in line.split("\n"):
                out.append(f"{indent}//{part.rstrip()}" if part.strip() else f"{indent}//")
        return out

    def rust_path(self, item, name=None) -> str:
        parts = [self.crate.lib or self.crate.bins[0]]
        parts += [p for p in self.file["module"].split("::") if p]
        parts += [p for p in item["inline_module"].split("::") if p]
        if item.get("owner"):
            parts.append(item["owner"].split("::")[-1])
        parts.append(name or item["name"])
        return "::".join(parts)

    def field_name(self, field, index) -> str:
        if field["name"] is None:
            return f"F{index}"
        name = pascal(field["name"])
        if field["vis"] != "pub":
            name = lower_first(name)
            if name in GO_PREDECLARED:
                name += "_"
        return name

    def field_tag(self, field, arc: bool) -> str:
        parts = []
        if field["name"] is not None:
            rs_tag = field["name"] + (",arc" if arc else "")
            parts.append(f'rs:"{rs_tag}"')
        elif arc:
            parts.append('rs:",arc"')
        serde = []
        for group in field.get("serde", []):
            for attr in split_attrs(group):
                key, _, value = attr.partition("=")
                key = key.strip()
                value = value.strip()
                if value.startswith('"') and value.endswith('"'):
                    value = value[1:-1]
                serde.append(f"{key}={value}" if value else key)
        if serde:
            text = ",".join(serde).replace("\\", "\\\\").replace('"', '\\"')
            parts.append(f'serde:"{text}"')
        return (" `" + " ".join(parts) + "`") if parts else ""

    def is_shared(self, node) -> bool:
        """Whether the field's type holds an `Arc`/`Rc` that a clone must share."""
        if not isinstance(node, dict):
            return False
        if node.get("k") == "path":
            hit = self.resolve(node["path"])
            if hit and hit[0] == "extern" and hit[1].replace("alloc::", "std::") in WRAPPERS_SHARED:
                return True
            return any(self.is_shared(arg) for arg in node.get("args", []))
        if node.get("k") in ("ref", "slice", "array", "ptr"):
            return self.is_shared(node["t"])
        if node.get("k") == "tuple":
            return any(self.is_shared(e) for e in node["elems"])
        return False

    def emit_fields(self, fields, exported_all=False) -> list[str]:
        out = []
        for index, field in enumerate(fields):
            out += self.docs(field["docs"], "\t")
            if field.get("cfg"):
                out.append(f"\t// PORT: #[cfg({'; '.join(field['cfg'])})]")
            name = self.field_name(field, index)
            if exported_all and field["name"] is not None:
                name = pascal(field["name"])
            out.append(f"\t{name} {self.go_type(field['ty'])}{self.field_tag(field, self.is_shared(field['ty']))}")
        return out

    def type_params(self, generics, declare=True) -> str:
        params = [p["name"] for p in generics["params"] if "const" not in p]
        if not params:
            return ""
        if declare:
            return "[" + ", ".join(f"{p} any" for p in params) + "]"
        return "[" + ", ".join(params) + "]"

    def serde_container(self, item) -> list[str]:
        attrs = []
        for group in item.get("serde", []):
            if re.match(r"^[A-Za-z_0-9]*: ", group):
                continue
            attrs += split_attrs(group)
        return attrs

    def quote_attrs(self, attrs) -> str:
        return "".join(", `" + a.replace("`", "'") + "`" for a in attrs)

    def emit_struct(self, item) -> list[str]:
        decl = item["decl"]
        name = self.ws.go_name(self.crate, self.file, item)
        out = self.docs(item["docs"])
        self.generic_scope.append({p["name"] for p in decl["generics"]["params"]})
        shape = self.item_shape(item)
        attrs = self.quote_attrs(self.serde_container(item))
        params = self.type_params(decl["generics"])
        inst = ""
        if params:
            inst = "[" + ", ".join("rs.Unit" for p in decl["generics"]["params"] if "const" not in p) + "]"
        self.used.add("rs")
        if shape == "newtype":
            out.append(f"type {name} {self.go_type(decl['fields'][0]['ty'])}")
            out.append("")
            out.append(f'var _ = rs.RegisterNewtype[{name}]("{item["name"]}"{attrs})')
        elif decl["fields_kind"] == "unit":
            out.append(f"type {name}{params} struct{{}}")
            out.append("")
            out.append(f'var _ = rs.RegisterUnitStruct[{name}{inst}]("{item["name"]}"{attrs})')
        else:
            out.append(f"type {name}{params} struct {{")
            out += self.emit_fields(decl["fields"])
            out.append("}")
            out.append("")
            register = "RegisterTupleStruct" if decl["fields_kind"] == "tuple" else "RegisterStruct"
            out.append(f'var _ = rs.{register}[{name}{inst}]("{item["name"]}"{attrs})')
        self.generic_scope.pop()
        if item["derives"]:
            out.insert(len(self.docs(item["docs"])), f"// derive: {', '.join(item['derives'])}")
        return out + [""] + self.error_method(item, name, self.type_params(decl["generics"], declare=False))

    def variant_attrs(self, item, variant) -> list[str]:
        attrs = []
        for group in variant.get("serde", []):
            attrs += split_attrs(group)
        return attrs

    def emit_enum(self, item) -> list[str]:
        decl = item["decl"]
        name = self.ws.go_name(self.crate, self.file, item)
        out = self.docs(item["docs"])
        if item["derives"]:
            out.append(f"// derive: {', '.join(item['derives'])}")
        self.used.add("rs")
        container = self.serde_container(item)
        container_spec = f"\n\trs.EnumAttrs({self.quote_attrs(container)[2:]})," if container else ""
        self.generic_scope.append({p["name"] for p in decl["generics"]["params"]})
        params = self.type_params(decl["generics"])
        inst = ""
        if params:
            inst = "[" + ", ".join("rs.Unit" for p in decl["generics"]["params"] if "const" not in p) + "]"
        use = self.type_params(decl["generics"], declare=False)
        if self.item_shape(item) == "cenum":
            repr_type = {"u8": "uint8", "u16": "uint16", "u32": "uint32", "u64": "uint64", "i8": "int8",
                         "i16": "int16", "i32": "int32", "i64": "int64", "usize": "int"}.get(decl.get("repr") or "", "uint8")
            out.append(f"type {name} {repr_type}")
            out.append("")
            out.append("const (")
            explicit = any(v["discriminant"] for v in decl["variants"])
            for index, variant in enumerate(decl["variants"]):
                out += self.docs(variant["docs"], "\t")
                constant = f"{name}_{variant['name']}"
                if variant["discriminant"]:
                    out.append(f"\t{constant} {name} = {variant['discriminant']}")
                elif index == 0:
                    out.append(f"\t{constant} {name} = iota")
                elif explicit:
                    out.append(f"\t{constant} {name} = {name}_{decl['variants'][index - 1]['name']} + 1")
                else:
                    out.append(f"\t{constant}")
            out.append(")")
            out.append("")
            specs = "".join(
                f'\n\trs.Const({name}_{v["name"]}, "{v["name"]}"{self.quote_attrs(self.variant_attrs(item, v))}),'
                for v in decl["variants"]
            )
            out.append(f'var _ = rs.RegisterEnum[{name}]("{item["name"]}",{container_spec}{specs}\n)')
            default = next((v for v in decl["variants"] if v["default"]), None)
            if default and default is not decl["variants"][0]:
                out.append("")
                out.append(f"var _ = rs.RegisterDefault(func() {name} {{ return {name}_{default['name']} }})")
            self.generic_scope.pop()
            return out + [""] + self.error_method(item, name, "")
        marker = "is" + name[:1].upper() + name[1:].replace("_", "")
        methods = self.enum_methods(item)
        out.append(f"type {name}{params} interface {{")
        out.append(f"\t{marker}()")
        if id(item) in self.error_types():
            out.append("\terror")
        for signature in methods:
            out.append(f"\t{signature}")
        out.append("}")
        out.append("")
        specs = ""
        for variant in decl["variants"]:
            variant_name = f"{name}_{variant['name']}"
            out += self.docs(variant["docs"])
            if variant.get("cfg"):
                out.append(f"// PORT: #[cfg({'; '.join(variant['cfg'])})]")
            if variant["kind"] == "unit":
                out.append(f"type {variant_name}{params} struct{{}}")
            else:
                out.append(f"type {variant_name}{params} struct {{")
                out += self.emit_fields(variant["fields"], exported_all=True)
                out.append("}")
            out.append("")
            out.append(f"func (*{variant_name}{use}) {marker}() {{}}")
            out.append("")
            shape = {"unit": "rs.ShapeUnit", "tuple": "rs.ShapeTuple", "named": "rs.ShapeStruct"}[variant["kind"]]
            specs += f'\n\trs.Variant[{variant_name}{inst}]("{variant["name"]}", {shape}{self.quote_attrs(self.variant_attrs(item, variant))}),'
        out.append(f'var _ = rs.RegisterEnum[{name}{inst}]("{item["name"]}",{container_spec}{specs}\n)')
        default = next((v for v in decl["variants"] if v["default"]), None)
        if default:
            out.append("")
            out.append(f"var _ = rs.RegisterDefault(func() {name} {{ return &{name}_{default['name']}{{}} }})")
        self.generic_scope.pop()
        return out + [""] + self.error_method(item, name, use)

    def error_types(self) -> dict:
        """Items of this crate that functions return as the error of a `Result`,
        or that implement `std::error::Error`: id(item) -> has a Display impl.

        Go's `error` needs an `Error() string` method, which Rust does not ask
        of a type used as `E`.
        """
        cache = self.crate.__dict__.setdefault("_error_types", None)
        if cache is not None:
            return cache
        found = {}
        display = set()
        for file in self.crate.files:
            if file["target"] not in ("lib", "bin"):
                continue
            other = self if file is self.file else Generator(self.ws, file["path"])
            for item in file["items"]:
                decl = item.get("decl") or {}
                if item["kind"] == "impl" and decl.get("trait") and decl.get("self_ty", {}).get("k") == "path":
                    trait = decl["trait"]["path"].split("::")[-1]
                    if trait in ("Display", "Error"):
                        hit = other.resolve(decl["self_ty"]["path"], file, self.crate)
                        if hit and hit[0] == "item":
                            if trait == "Display":
                                display.add(id(hit[3]))
                            else:
                                found[id(hit[3])] = hit[3]
                output = decl.get("output") if item["kind"] in ("fn", "method", "trait_fn") else None
                if not output or output.get("k") != "path" or len(output.get("args", [])) < 2:
                    continue
                if output["path"].split("::")[-1] != "Result":
                    continue
                error = output["args"][1]
                if error.get("k") == "path":
                    hit = other.resolve(error["path"], file, self.crate)
                    if hit and hit[0] == "item" and hit[1] is self.crate and hit[3]["kind"] in ("struct", "enum"):
                        found[id(hit[3])] = hit[3]
        cache = {key: (key in display) for key in found}
        self.crate.__dict__["_error_types"] = cache
        return cache

    def error_method(self, item, name: str, use: str) -> list[str]:
        """`Error() string` for a type used as an error."""
        errors = self.error_types()
        if id(item) not in errors:
            return []
        self.used.add("rs")
        text = "rs.ToString" if errors[id(item)] else "rs.DebugString"
        why = "its Display text" if errors[id(item)] else "its Debug text, as it has no Display"
        shape = self.item_shape(item)
        if shape == "enum":
            lines = [f"// Error makes each variant of {name} a Go error: {why}."]
            for variant in item["decl"]["variants"]:
                lines.append(f"func (v *{name}_{variant['name']}{use}) Error() string {{ return {text}(v) }}")
            return lines + [""]
        receiver = "" if shape in ("copy", "cenum", "newtype") else "*"
        return [
            f"// Error makes {name} a Go error: {why}.",
            f"func (e {receiver}{name}{use}) Error() string {{ return {text}(e) }}", "",
        ]

    def impls_of(self, target_item):
        """Every impl block in the crate whose self type is target_item: (file, impl item, methods)."""
        found = []
        for file in self.crate.files:
            if file["target"] not in ("lib", "bin") or file["test_only"]:
                continue
            items = file["items"]
            for index, item in enumerate(items):
                if item["kind"] != "impl" or item["test_only"]:
                    continue
                self_ty = (item.get("decl") or {}).get("self_ty") or {}
                if self_ty.get("k") != "path":
                    continue
                if self_ty["path"].split("::")[-1] != target_item["name"]:
                    continue
                hit = self.resolve(self_ty["path"], file, self.crate)
                if not hit or hit[0] != "item" or hit[3] is not target_item:
                    continue
                methods = [
                    m for m in items[index + 1:]
                    if m["kind"] == "method" and m.get("owner") == item["name"]
                    and item["start"] <= m["start"] <= item["end"]
                ]
                found.append((file, item, methods))
        return found

    def enum_methods(self, enum_item) -> list[str]:
        """Interface entries for the methods a data-carrying enum has."""
        signatures = []
        for file, impl, methods in self.impls_of(enum_item):
            other = Generator(self.ws, file["path"]) if file is not self.file else self
            saved = other.self_type
            other.self_type = ("self", self.ws.go_name(self.crate, self.file, enum_item))
            for method in methods:
                if not method["has_self"]:
                    continue
                name, params, results, generics = other.signature(method, impl)
                if generics:
                    # A method with its own type parameters is a function in Go.
                    continue
                if name in self.variant_field_names(enum_item):
                    name += "Method"
                signatures.append(f"{name}({params}){results}")
            other.self_type = saved
            self.used |= other.used
        return signatures

    @staticmethod
    def variant_field_names(enum_item) -> set[str]:
        names = set()
        for variant in enum_item["decl"]["variants"]:
            for index, field in enumerate(variant["fields"]):
                names.add(pascal(field["name"]) if field["name"] is not None else f"F{index}")
        return names

    def method_go_name(self, method, impl) -> str:
        trait = (impl.get("decl") or {}).get("trait") if impl else None
        if trait:
            key = (trait["path"].split("::")[-1], method["name"])
            if key in STD_TRAIT_METHODS:
                return STD_TRAIT_METHODS[key]
            return pascal(method["name"])
        name = pascal(method["name"])
        if method["vis"] != "pub":
            name = lower_first(name)
            if name in portmap.GO_KEYWORDS:
                name += "_"
        return name

    def bound_substitutions(self, generics) -> tuple[dict, list[str]]:
        """Generic parameters that Go spells as a plain type.

        `F: FnOnce() -> R` is a `func() R`, `F: Future<Output = T>` is a
        `tokio.Fn[T]`, `P: AsRef<Path>` is a `string`. Returns the
        substitutions and the type parameters that remain.
        """
        bounds = {p["name"]: list(p.get("bounds", [])) for p in generics["params"] if "const" not in p}
        for extra in generics.get("where_bounds", []):
            ty = extra["ty"]
            if ty.get("k") == "path" and ty["path"] in bounds:
                bounds[ty["path"]] += extra["bounds"]
        substitutions = {}
        remaining = []
        for name, list_ in bounds.items():
            node = {"bounds": list_, "text": name}
            replaced = None
            for bound in list_:
                last = bound["path"].split("::")[-1]
                if last in ("Fn", "FnMut", "FnOnce") and bound.get("paren"):
                    replaced = ("fn", bound["paren"])
                elif last == "Future":
                    output = next((v for k, v in bound["assoc"] if k == "Output"), None)
                    replaced = ("future", output)
                elif last in ("Into", "AsRef", "Borrow") and bound["args"]:
                    replaced = ("type", bound["args"][0])
                elif last == "IntoIterator":
                    item_type = next((v for k, v in bound["assoc"] if k == "Item"), None)
                    if item_type is not None:
                        replaced = ("slice", item_type)
                elif last in ("ToString", "Display"):
                    replaced = ("any", None)
                if replaced:
                    break
            if replaced is None:
                remaining.append(name)
            else:
                substitutions[name] = replaced
        return substitutions, remaining

    def substituted(self, name: str) -> str | None:
        for scope in reversed(self.substitution_scope):
            if name in scope:
                kind, value = scope[name]
                if kind == "fn":
                    return self.func_type(value["inputs"], value["output"])
                if kind == "future":
                    self.used.add("tokio")
                    inner = self.go_type(value, "generic") if value is not None else name + "Output"
                    return f"tokio.Fn[{inner}]"
                if kind == "type":
                    return self.go_type(value, "param")
                if kind == "slice":
                    return f"[]{self.go_type(value, 'generic')}"
                return "any"
        return None

    def signature(self, item, impl=None):
        """(go name, params text, results text, generic params) of a function or method."""
        decl = item["decl"]
        substitutions, remaining = self.bound_substitutions(decl["generics"])
        extra_params = [
            name + "Output" for name, (kind, value) in substitutions.items() if kind == "future" and value is None
        ]
        self.substitution_scope.append(substitutions)
        self.generic_scope.append(set(remaining) | set(extra_params))
        params = []
        if decl["async"]:
            self.used.add("context")
            params.append("ctx context.Context")
        seen = {"ctx"} if decl["async"] else set()
        for index, argument in enumerate(decl["inputs"]):
            raw = argument["name"]
            raw = re.sub(r"^(mut|ref)\s+", "", raw)
            name = camel(raw) if re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", raw) else f"arg{index}"
            while name in seen:
                name += "_"
            seen.add(name)
            params.append(f"{name} {self.go_type(argument['ty'], 'param')}")
        results = self.result_types(decl["output"])
        if decl["output"] and decl["output"].get("k") == "impl" and self.future_output(decl["output"]) is None and False:
            pass
        text = ""
        if len(results) == 1:
            text = " " + results[0]
        elif results:
            text = " (" + ", ".join(results) + ")"
        names = remaining + extra_params
        generics = "[" + ", ".join(f"{n} any" for n in names) + "]" if names else ""
        self.generic_scope.pop()
        self.substitution_scope.pop()
        name = self.method_go_name(item, impl) if item["kind"] in ("method", "trait_fn") else None
        self.last_param_names = [p.split(" ", 1)[0] for p in params]
        return name, ", ".join(params), text, generics

    def todo_body(self, item) -> list[str]:
        self.used.add("rs")
        return [f'\tpanic(rs.Todo("{self.rust_path(item)}"))', "}"]

    def emit_fn(self, item) -> list[str]:
        name = self.ws.go_name(self.crate, self.file, item)
        _, params, results, generics = self.signature(item)
        out = self.docs(item["docs"])
        out.append(f"// rust: {item['signature']}")
        out.append(f"func {name}{generics}({params}){results} {{")
        return out + self.todo_body(item) + [""]

    def emit_impl(self, impl, methods) -> list[str]:
        decl = impl["decl"]
        self_ty = decl["self_ty"]
        out = []
        trait = decl.get("trait")
        trait_text = f"{trait['path']} for " if trait else ""
        out.append(f"// impl {trait_text}{self_ty.get('text', '?')}")
        out.append("")
        target = self.resolve(self_ty["path"]) if self_ty.get("k") == "path" else None
        impl_generics = {p["name"] for p in decl["generics"]["params"]}
        self.generic_scope.append(impl_generics)
        if not target or target[0] != "item":
            for method in methods:
                out += self.docs(method["docs"])
                out.append(f"// PORT: impl for {self_ty.get('text', '?')}, a type this crate does not own.")
                out.append(f"// rust: {method['signature']}")
                prefix = self.ws.module_go_prefix(self.crate, self.file, method)
                name = lower_first(prefix) + "_" + (trait["path"].split("::")[-1] + "_" if trait else "") + method["name"]
                _, params, results, generics = self.signature(method, impl)
                receiver = self.go_type(self_ty, "param")
                first = f"self_ {receiver}" + (", " if params else "")
                out.append(f"func {name}{generics}({first}{params}){results} {{")
                out += self.todo_body(method) + [""]
            self.generic_scope.pop()
            return out
        _, target_crate, target_file, target_item = target
        go_type = self.go_item_name(target_crate, target_file, target_item)
        shape = self.item_shape(target_item)
        type_generics = target_item.get("decl", {}).get("generics", {"params": []})
        declare = self.type_params(type_generics)
        use = self.type_params(type_generics, declare=False)
        saved_self = self.self_type
        self.self_type = ("self", go_type + use)
        field_names = set()
        if target_item["kind"] == "struct":
            for index, field in enumerate(target_item["decl"]["fields"]):
                field_names.add(self.field_name(field, index))
        elif shape == "enum":
            field_names = self.variant_field_names(target_item)
        trait_name = trait["path"].split("::")[-1] if trait else None
        for method in methods:
            out += self.docs(method["docs"])
            out.append(f"// rust: {method['signature']}")
            name, params, results, generics = self.signature(method, impl)
            param_names = list(self.last_param_names)
            if name in field_names:
                name += "Method"
            if not method["has_self"]:
                base = go_type.split(".")[-1]
                fn_name = base + "_" + method["name"]
                if trait and trait["args"]:
                    # `impl From<A> for T` and `impl From<B> for T` are two functions.
                    source = self.go_type(trait["args"][0], "generic").split("/*")[0]
                    source = re.sub(r"[^A-Za-z0-9_]", "", source.split(".")[-1])
                    fn_name += "_" + source
                if (method["vis"] != "pub" and not trait) or not base[:1].isupper():
                    fn_name = lower_first(fn_name)
                both = declare
                if generics:
                    both = "[" + ", ".join(filter(None, [declare.strip("[]"), generics.strip("[]")])) + "]"
                out.append(f"func {fn_name}{both}({params}){results} {{")
                out += self.todo_body(method) + [""]
                continue
            if shape == "enum" or generics:
                # Go cannot put a method on an interface or give a method its
                # own type parameters: the body is a function, and each
                # variant forwards to it.
                base = go_type.split(".")[-1]
                fn_name = base + "_" + method["name"]
                if trait:
                    fn_name = base + "_" + trait_name + "_" + method["name"]
                if (method["vis"] != "pub" and not trait) or not base[:1].isupper():
                    fn_name = lower_first(fn_name)
                both = declare
                if generics:
                    both = "[" + ", ".join(filter(None, [declare.strip("[]"), generics.strip("[]")])) + "]"
                receiver_type = go_type + use if shape == "enum" else "*" + go_type + use
                first = f"self_ {receiver_type}" + (", " if params else "")
                out.append(f"func {fn_name}{both}({first}{params}){results} {{")
                out += self.todo_body(method) + [""]
                if shape == "enum" and not generics:
                    call_args = ", ".join(param_names)
                    ret = "return " if results else ""
                    for variant in target_item["decl"]["variants"]:
                        variant_name = f"{go_type}_{variant['name']}"
                        call = f"{fn_name}{use}(v" + (", " + call_args if call_args else "") + ")"
                        out.append(f"func (v *{variant_name}{use}) {name}({params}){results} {{ {ret}{call} }}")
                    out.append("")
                continue
            receiver = "s"
            taken = set(param_names)
            while receiver in taken:
                receiver += "_"
            star = "" if shape in ("copy", "cenum", "newtype") and method["decl"]["receiver"]["kind"] != "mut" else "*"
            out.append(f"func ({receiver} {star}{go_type}{use}) {name}({params}){results} {{")
            out += self.todo_body(method) + [""]
            if trait_name == "Display" and method["name"] == "fmt":
                out.append(f"func ({receiver} {star}{go_type}{use}) String() string {{ return rs.ToString({receiver}) }}")
                out.append("")
        self.self_type = saved_self
        self.generic_scope.pop()
        return out

    def emit_trait(self, item, methods) -> list[str]:
        decl = item["decl"]
        name = self.ws.go_name(self.crate, self.file, item)
        self.generic_scope.append({p["name"] for p in decl["generics"]["params"]} | set(decl.get("assoc_types", [])))
        params = self.type_params(decl["generics"])
        out = self.docs(item["docs"])
        if decl.get("assoc_types"):
            out.append(f"// PORT: associated types {', '.join(decl['assoc_types'])}")
        saved_self = self.self_type
        self.self_type = ("self", name)
        out.append(f"type {name}{params} interface {{")
        defaults = []
        for method in methods:
            out += self.docs(method["docs"], "\t")
            method_name = pascal(method["name"])
            _, method_params, results, generics = self.signature(method, None)
            if not method["has_self"] or generics:
                out.append(f"\t// PORT: not a method in Go: {method['signature']}")
                continue
            out.append(f"\t{method_name}({method_params}){results}")
            if method["decl"]["has_body"]:
                defaults.append((method, method_name, method_params, results))
        out.append("}")
        out.append("")
        for method, method_name, method_params, results in defaults:
            out.append(f"// {name}_default_{method['name']} is the default body of {item['name']}::{method['name']}.")
            out.append(f"// rust: {method['signature']}")
            first = f"self_ {name}" + (", " if method_params else "")
            out.append(f"func {name}_default_{method['name']}({first}{method_params}){results} {{")
            out += self.todo_body(method) + [""]
        self.self_type = saved_self
        self.generic_scope.pop()
        return out

    INT_LIMITS = {
        "u8::MAX": "math.MaxUint8", "u16::MAX": "math.MaxUint16", "u32::MAX": "math.MaxUint32",
        "u64::MAX": "math.MaxUint64", "i8::MAX": "math.MaxInt8", "i16::MAX": "math.MaxInt16",
        "i32::MAX": "math.MaxInt32", "i64::MAX": "math.MaxInt64", "i8::MIN": "math.MinInt8",
        "i16::MIN": "math.MinInt16", "i32::MIN": "math.MinInt32", "i64::MIN": "math.MinInt64",
        "u8::MIN": "0", "u16::MIN": "0", "u32::MIN": "0", "u64::MIN": "0", "usize::MIN": "0",
        "usize::MAX": "rs.UsizeMax", "isize::MAX": "rs.UsizeMax", "f64::MAX": "math.MaxFloat64",
        "f32::MAX": "math.MaxFloat32", "f64::INFINITY": "math.Inf(1)", "f64::NEG_INFINITY": "math.Inf(-1)",
        "f64::EPSILON": "0x1p-52", "Duration::ZERO": "time.Duration(0)", "Duration::MAX": "time.Duration(math.MaxInt64)",
    }
    DURATION_UNITS = {
        "from_secs": "time.Second", "from_millis": "time.Millisecond", "from_micros": "time.Microsecond",
        "from_nanos": "time.Nanosecond",
    }
    BASIC_GO_TYPES = {
        "string", "bool", "int", "uint8", "uint16", "uint32", "uint64", "int8", "int16", "int32", "int64",
        "float32", "float64", "time.Duration", "rune", "byte", "uint",
    }

    def find_value_item(self, path: str):
        """The const or static a path names: (crate, file, item) or None."""
        segments = [s for s in path.split("::") if s]
        name = segments[-1]
        if len(segments) == 1:
            for item in self.file["items"]:
                if item["kind"] in ("const", "static") and item["name"] == name and not item["test_only"]:
                    return self.crate, self.file, item
        candidates = []
        for use in self.file["uses"]:
            parts = use["path"].split("::")
            exported = use["alias"] or parts[-1]
            if not use["glob"] and exported == segments[0]:
                candidates.append(parts + segments[1:])
            elif use["glob"]:
                candidates.append(parts + segments)
        candidates.append(segments)
        for candidate in candidates:
            target = self.ws._absolute(self.crate, self.file, candidate)
            if target is None and len(candidate) > 1:
                base = [p for p in self.file["module"].split("::") if p]
                target = ("crate", self.crate, base + candidate)
            if not target or target[0] != "crate":
                continue
            owner, rest = target[1], target[2]
            file, left = self._module_file(owner, rest)
            if file is None or not left:
                continue
            if len(left) == 1:
                for item in file["items"]:
                    if item["kind"] in ("const", "static") and item["name"] == left[0] and not item["test_only"]:
                        return owner, file, item
            if len(left) == 2:
                # `Type::CONSTANT`
                for item in file["items"]:
                    if item["kind"] == "assoc_const" and item["name"] == left[1] and (item.get("owner") or "").split("::")[-1] == left[0]:
                        return owner, file, item
        return None

    def find_callable(self, path: str):
        """The Go name of the workspace function a path calls, or None."""
        segments = [s for s in path.split("::") if s]
        candidates = [segments]
        for use in self.file["uses"]:
            parts = use["path"].split("::")
            exported = use["alias"] or parts[-1]
            if not use["glob"] and exported == segments[0]:
                candidates.insert(0, parts + segments[1:])
            elif use["glob"]:
                candidates.append(parts + segments)
        if len(segments) == 1:
            for item in self.file["items"]:
                if item["kind"] == "fn" and item["name"] == segments[0] and not item["test_only"] and not item.get("fn_scope"):
                    return self.ws.go_name(self.crate, self.file, item)
        for candidate in candidates:
            target = self.ws._absolute(self.crate, self.file, candidate)
            if target is None and len(candidate) > 1:
                base = [p for p in self.file["module"].split("::") if p]
                target = ("crate", self.crate, base + candidate)
            if not target or target[0] != "crate":
                continue
            owner, rest = target[1], target[2]
            file, left = self._module_file(owner, rest)
            if file is None or not left or len(left) > 2:
                continue
            for item in file["items"]:
                if item["test_only"]:
                    continue
                free = len(left) == 1 and item["kind"] == "fn" and item["name"] == left[0] and not item.get("fn_scope")
                assoc = (
                    len(left) == 2 and item["kind"] == "method" and not item["has_self"]
                    and item["name"] == left[1] and (item.get("owner") or "").split("::")[-1] == left[0]
                )
                if not (free or assoc):
                    continue
                if assoc:
                    hit = self.resolve(item["owner"], file, owner)
                    if not hit or hit[0] != "item":
                        continue
                    name = self.ws.go_name(hit[1], hit[2], hit[3]) + "_" + item["name"]
                    if item["vis"] != "pub" or not name[:1].isupper():
                        name = lower_first(name)
                else:
                    name = self.ws.go_name(owner, file, item)
                if owner is not self.crate:
                    self.used.add(owner.go_package)
                    return f"{owner.go_package}.{name}"
                return name
        return None

    def value_go_name(self, crate, file, item) -> str:
        if item["kind"] == "assoc_const":
            hit = self.resolve(item["owner"], file, crate)
            base = self.ws.go_name(hit[1], hit[2], hit[3]) if hit and hit[0] == "item" else item["owner"].split("::")[-1]
            name = base + "_" + item["name"]
            if item["vis"] != "pub" or not name[:1].isupper():
                name = lower_first(name)
        else:
            name = self.ws.go_name(crate, file, item)
        if crate is not self.crate:
            self.used.add(crate.go_package)
            return f"{crate.go_package}.{name}"
        return name

    def go_expr(self, node, want: str | None = None, depth: int = 0):
        """(Go text, is a Go constant expression) for an initializer, or None
        when it needs a person."""
        if node is None or depth > 40:
            return None
        kind = node.get("k")
        if kind == "lit":
            lit = node["kind"]
            if lit == "str":
                return go_string(node["value"]), True
            if lit in ("int", "float", "bool"):
                return node["value"], True
            if lit == "char":
                return node["value"], True
            if lit == "bytes":
                holder = want if want and re.fullmatch(r"\[\d*\](byte|uint8)", want) else "[]byte"
                return holder + "{" + ", ".join(str(b) for b in node["value"]) + "}", False
            return None
        if kind == "paren":
            inner = self.go_expr(node["e"], want, depth + 1)
            return (f"({inner[0]})", inner[1]) if inner else None
        if kind == "un":
            inner = self.go_expr(node["e"], want, depth + 1)
            if not inner or node["op"] not in ("-", "!"):
                return None
            return f"{node['op']}{inner[0]}", inner[1]
        if kind == "bin":
            op = node["op"]
            if op not in ("+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^", "==", "!=", "<", ">", "<=", ">=", "&&", "||"):
                return None
            left = self.go_expr(node["l"], want, depth + 1)
            right = self.go_expr(node["r"], want, depth + 1)
            if not left or not right:
                return None
            return f"{left[0]} {op} {right[0]}", left[1] and right[1]
        if kind == "path":
            path = node["path"]
            if path in ("true", "false"):
                return path, True
            if path in self.INT_LIMITS:
                text = self.INT_LIMITS[path]
                self.mark(text)
                return text, "(" not in text
            if path == "None" and want and want.startswith("rs.Option["):
                self.used.add("rs")
                return f"rs.None[{want[len('rs.Option['):-1]}]()", False
            hit = self.find_value_item(path)
            if hit:
                return self.value_go_name(*hit), self.constable(*hit, depth + 1)
            # `Enum::Variant` of a data-free enum is a constant.
            segments = path.split("::")
            if len(segments) >= 2:
                owner = self.resolve("::".join(segments[:-1]))
                if owner and owner[0] == "item" and owner[3]["kind"] == "enum":
                    name = self.go_item_name(owner[1], owner[2], owner[3]) + "_" + segments[-1]
                    variant = next((v for v in owner[3]["decl"]["variants"] if v["name"] == segments[-1]), None)
                    if variant is None:
                        return None
                    if self.item_shape(owner[3]) == "cenum":
                        return name, True
                    if variant["kind"] == "unit":
                        return f"&{name}{{}}", False
            return None
        if kind == "cast":
            inner = self.go_expr(node["e"], None, depth + 1)
            if not inner:
                return None
            target = self.go_type(node["ty"], "generic")
            if target not in self.BASIC_GO_TYPES:
                return None
            return f"{target}({inner[0]})", inner[1]
        if kind == "call":
            function = node["f"]
            if function.get("k") != "path":
                return None
            path = function["path"]
            last = path.split("::")[-1]
            args = node["args"]
            if path.split("::")[0] in ("Duration", "std") and last in self.DURATION_UNITS and len(args) == 1:
                inner = self.go_expr(args[0], None, depth + 1)
                if not inner:
                    return None
                self.used.add("time")
                value = inner[0] if re.fullmatch(r"[0-9]+", inner[0]) else f"time.Duration({inner[0]})"
                return f"{value} * {self.DURATION_UNITS[last]}", inner[1]
            if path == "Some" and len(args) == 1:
                inner_want = want[len("rs.Option["):-1] if want and want.startswith("rs.Option[") else None
                inner = self.go_expr(args[0], inner_want, depth + 1)
                if not inner:
                    return None
                self.used.add("rs")
                return (f"rs.Some[{inner_want}]({inner[0]})" if inner_want else f"rs.Some({inner[0]})"), False
            if path in ("String::new", "PathBuf::new") and not args:
                return '""', True
            if path in ("String::from", "PathBuf::from", "Path::new", "OsString::from", "OsStr::new") and len(args) == 1:
                return self.go_expr(args[0], want, depth + 1)
            if path in ("Vec::new", "Vec::with_capacity"):
                return "nil", False
            # Constructors of the standard library's synchronisation types.
            head = path.replace("std::sync::atomic::", "").replace("std::sync::", "")
            if head.endswith("::new") and want:
                owner = head[: -len("::new")]
                if owner in ("OnceLock", "Mutex", "RwLock", "LazyLock", "Condvar", "Once") or owner.startswith("Atomic"):
                    translated = []
                    inner_want = want[want.index("[") + 1 : -1] if "[" in want and want.endswith("]") else None
                    for arg in args:
                        part = self.go_expr(arg, inner_want, depth + 1)
                        if not part:
                            return None
                        translated.append(part[0])
                    self.used.add("rssync")
                    base, _, type_args = want.partition("[")
                    type_args = "[" + type_args if type_args else ""
                    return f"{base.lstrip('*')}New{type_args}({', '.join(translated)})", False
            # A function or associated function of this workspace.
            callable_ = self.find_callable(path)
            if callable_:
                translated = []
                for arg in args:
                    part = self.go_expr(arg, None, depth + 1)
                    if not part:
                        return None
                    translated.append(part[0])
                return f"{callable_}({', '.join(translated)})", False
            return None
        if kind == "method":
            if node["name"] in ("to_string", "to_owned", "into", "as_str", "to_path_buf", "as_ref", "clone") and not node["args"]:
                return self.go_expr(node["recv"], want, depth + 1)
            if node["name"] == "as_bytes" and not node["args"]:
                inner = self.go_expr(node["recv"], None, depth + 1)
                return (f"[]byte({inner[0]})", False) if inner else None
            return None
        if kind == "ref":
            return self.go_expr(node["e"], want, depth + 1)
        if kind in ("array", "tuple"):
            elems = node["elems"]
            if kind == "tuple":
                if not want or not want.startswith("rs.Tuple") or len(elems) > 5:
                    return None
                # The tuple's element types come from the declared type, so
                # an untyped constant gets the width Rust gave it.
                element_types = split_attrs(want[want.index("[") + 1 : -1])
                if len(element_types) != len(elems):
                    return None
                parts = [self.go_expr(e, t, depth + 1) for e, t in zip(elems, element_types)]
                if not all(parts):
                    return None
                self.used.add("rs")
                fields = ", ".join(f"F{i}: {p[0]}" for i, p in enumerate(parts))
                return f"{want}{{{fields}}}", False
            if not want:
                return None
            element = None
            if want.startswith("[]"):
                element = want[2:]
            else:
                match = re.match(r"\[[^\]]*\](.*)", want)
                element = match.group(1) if match else None
            if element is None:
                return None
            parts = [self.go_expr(e, element, depth + 1) for e in elems]
            if not all(parts):
                return None
            if len(parts) > 4:
                body = "\n" + "".join(f"\t{p[0]},\n" for p in parts)
            else:
                body = ", ".join(p[0] for p in parts)
            return f"{want}{{{body}}}", False
        if kind == "struct":
            hit = self.resolve(node["path"])
            if not hit or hit[0] != "item" or hit[3]["kind"] != "struct" or node.get("rest"):
                return None
            _, crate, file, item = hit
            fields = {f["name"]: (index, f) for index, f in enumerate(item["decl"]["fields"])}
            other = self if file is self.file else Generator(self.ws, file["path"])
            parts = []
            for name, value in node["fields"]:
                if name not in fields:
                    return None
                index, field = fields[name]
                field_type = other.go_type(field["ty"])
                self.used |= other.used
                part = self.go_expr(value, field_type, depth + 1)
                if not part:
                    return None
                parts.append(f"{self.field_name(field, index)}: {part[0]}")
            if len(parts) != len(fields):
                return None
            name = self.go_item_name(crate, file, item)
            body = "\n" + "".join(f"\t{p},\n" for p in parts) if len(parts) > 3 else ", ".join(parts)
            return f"{name}{{{body}}}", False
        if kind == "macro":
            name = node["name"]
            args = node.get("args")
            if name == "concat" and args is not None:
                parts = [self.go_expr(a, "string", depth + 1) for a in args]
                if not all(parts):
                    return None
                return " + ".join(p[0] for p in parts) or '""', all(p[1] for p in parts)
            if name in ("include_str", "include_bytes") and args and args[0].get("k") == "lit":
                import os
                target = os.path.normpath(os.path.join(os.path.dirname(self.path), args[0]["value"]))
                self.used.add("assets")
                call = "MustString" if name == "include_str" else "MustBytes"
                return f'assets.{call}("{target}")', False
            if name == "vec" and args is not None and want and want.startswith("[]"):
                parts = [self.go_expr(a, want[2:], depth + 1) for a in args]
                if not all(parts):
                    return None
                return f"{want}{{" + ", ".join(p[0] for p in parts) + "}", False
            return None
        return None

    def constable(self, crate, file, item, depth: int = 0) -> bool:
        """Whether a Rust const becomes a Go `const` (a basic type with a
        constant expression) rather than a `var`."""
        key = (file["path"], item["start"], item["name"])
        if key in self.constable_memo:
            return self.constable_memo[key]
        self.constable_memo[key] = False
        if item["kind"] == "static" or depth > 40:
            return False
        other = self if file is self.file else Generator(self.ws, file["path"])
        go_type = other.go_type(item["decl"]["ty"])
        result = False
        if go_type in self.BASIC_GO_TYPES:
            value = other.go_expr(item["decl"].get("tree"), go_type, depth + 1)
            result = bool(value and value[1])
        self.constable_memo[key] = result
        return result

    def emit_const(self, item) -> list[str]:
        decl = item["decl"]
        name = self.ws.go_name(self.crate, self.file, item)
        if item["kind"] == "assoc_const":
            owner = item["owner"].split("::")[-1]
            hit = self.resolve(item["owner"])
            base = self.go_item_name(hit[1], hit[2], hit[3]) if hit and hit[0] == "item" else owner
            name = base.split(".")[-1] + "_" + item["name"]
            if item["vis"] != "pub" or not name[:1].isupper():
                name = lower_first(name)
        out = self.docs(item["docs"])
        go_type = self.go_type(decl["ty"])
        expr = decl["expr"]
        value = self.go_expr(decl.get("tree"), go_type)
        if item.get("fn_scope"):
            out.append(f"// Declared inside `{item['fn_scope']}`.")
        if value is None:
            # No Go value could be derived. The name on the right does not
            # exist, so the file does not compile until a person writes it.
            self.notes += 1
            out.append(f"// rust: {expr[:600]}")
            out.append(f"var {name} {go_type} = PORT_VALUE")
            return out + [""]
        text, constant = value
        if constant and go_type in self.BASIC_GO_TYPES and item["kind"] != "static":
            out.append(f"const {name} {go_type} = {text}")
        else:
            out.append(f"var {name} {go_type} = {text}")
        return out + [""]

    def emit_alias(self, item) -> list[str]:
        decl = item["decl"]
        name = self.ws.go_name(self.crate, self.file, item)
        self.generic_scope.append({p["name"] for p in decl["generics"]["params"]})
        out = self.docs(item["docs"])
        params = self.type_params(decl["generics"])
        out.append(f"type {name}{params} = {self.go_type(decl['ty'])}")
        self.generic_scope.pop()
        return out + [""]

    def bucket_of(self, cfgs) -> tuple[str, list[str]]:
        """The build constraint an item's `cfg`s amount to ("" for none), and
        notes for the predicates Go cannot express at build time."""
        constraints = []
        notes = []
        for cfg in cfgs:
            constraint = cfg_constraint(cfg)
            if constraint is None:
                notes.append(f"// PORT: #[cfg({cfg})] has no build tag; decide it at run time or by hand.")
                self.notes += 1
            elif constraint not in constraints:
                constraints.append(constraint)
        wrapped = [f"({c})" if "||" in c and len(constraints) > 1 else c for c in constraints]
        return " && ".join(wrapped), notes

    def generate_files(self) -> dict[str, str]:
        """Go file path -> text. Items under a `cfg` go to a file of their own
        with the matching `//go:build` line."""
        items = [i for i in self.file["items"] if not i["test_only"]]
        file_constraint, file_notes = self.bucket_of(self.file["cfg"])
        buckets: dict[str, list[str]] = {"": list(file_notes)}

        def emit(constraint: str, lines: list[str]) -> None:
            buckets.setdefault(constraint, []).extend(lines)

        def own_cfg(item) -> tuple[str, list[str]]:
            return self.bucket_of(item.get("cfg", []))

        for position, item in enumerate(items):
            kind = item["kind"]
            constraint, notes = own_cfg(item)
            if kind == "struct":
                emit(constraint, notes + self.emit_struct(item))
            elif kind == "enum":
                emit(constraint, notes + self.emit_enum(item))
            elif kind == "fn" and not item.get("owner"):
                if item.get("fn_scope"):
                    notes.append(f"// Declared inside `{item['fn_scope']}`.")
                emit(constraint, notes + self.emit_fn(item))
            elif kind == "impl":
                members = [
                    m for m in items[position + 1:]
                    if m["kind"] in ("method", "assoc_const") and item["start"] <= m["start"] <= item["end"]
                    and m["inline_module"] == item["inline_module"]
                ]
                by_bucket: dict[str, list] = {}
                member_notes: dict[str, list[str]] = {}
                for member in members:
                    member_constraint, extra = own_cfg(member)
                    by_bucket.setdefault(member_constraint, []).append(member)
                    member_notes.setdefault(member_constraint, []).extend(extra)
                if not members:
                    by_bucket[constraint] = []
                for member_constraint, group in by_bucket.items():
                    methods = [m for m in group if m["kind"] == "method"]
                    lines = member_notes.get(member_constraint, []) + self.emit_impl(item, methods)
                    for const in (m for m in group if m["kind"] == "assoc_const"):
                        lines += self.emit_const(const)
                    emit(member_constraint, lines)
            elif kind == "trait":
                methods = [
                    m for m in items[position + 1:]
                    if m["kind"] == "trait_fn" and item["start"] <= m["start"] <= item["end"]
                ]
                emit(constraint, notes + self.emit_trait(item, methods))
            elif kind in ("const", "static"):
                emit(constraint, notes + self.emit_const(item))
            elif kind == "type":
                emit(constraint, notes + self.emit_alias(item))
            elif kind == "macro_rules":
                emit(constraint, [f"// PORT: macro_rules! {item['name']}: write the expansion out at each use.", ""])
            elif kind == "union":
                emit(constraint, [f"// PORT: union {item['name']}", ""])

        blob = subprocess.run(
            ["git", "rev-parse", f"HEAD:{self.path}"], cwd=portmap.ROOT, capture_output=True, text=True
        ).stdout.strip()
        base = self.ws.go_file(self.crate, self.file)
        target = "::".join([self.crate.lib or self.crate.bins[0], *[p for p in self.file["module"].split("::") if p]])
        prefix = self.ws.module_go_prefix(self.crate, self.file)
        logs = any(
            m.startswith("tracing::") or m in ("warn", "info", "debug", "error", "trace") for m in self.file["macros"]
        )
        workspace_packages = {c.go_package for c in self.ws.crates.values() if c.lib}
        files = {}
        for constraint, body in buckets.items():
            if constraint and not any(line.strip() for line in body):
                continue
            parts = [c for c in dict.fromkeys((file_constraint, constraint)) if c]
            if len(parts) > 1:
                parts = [f"({c})" if "||" in c and not c.startswith(("(", "!(")) else c for c in parts]
            full = " && ".join(parts)
            path = base if not constraint else base[: -len(".go")] + cfg_suffix(constraint) + ".go"
            head = []
            if full:
                head += [f"//go:build {full}", ""]
            head += [f"// Port of {self.path}", f"// rust-blob: {blob}", "", f"package {self.crate.go_package}", ""]
            text = "\n".join(body)
            code = re.sub(r"/\*.*?\*/", "", text)
            code = "\n".join(line for line in code.split("\n") if not line.lstrip().startswith("//"))
            imports = []
            for package in sorted(self.used):
                if package == self.crate.go_package or not re.search(rf"\b{re.escape(package)}\.", code):
                    continue
                if package in IMPORTS:
                    imports.append(f'\t"{IMPORTS[package]}"')
                elif package == "io":
                    imports.append('\t"io"')
                elif package == "acp":
                    imports.append('\tacp "github.com/BrokkAi/acp-go"')
                elif package in workspace_packages:
                    imports.append(f'\t"github.com/BrokkAi/mjolnir/go/{package}"')
                else:
                    imports.append(f'\t"github.com/BrokkAi/mjolnir/go/crates/{package}"')
            if imports:
                head += ["import ("] + imports + [")", ""]
            if not constraint and logs:
                const = lower_first(prefix) + "_TARGET"
                head += [f"// {const} is the tracing target of this file's events: its Rust module path.",
                         f'const {const} = "{target}"', ""]
            files[path] = "\n".join(head + body).rstrip() + "\n"
        return files

    def generate(self) -> str:
        files = self.generate_files()
        if len(files) == 1:
            return next(iter(files.values()))
        return "".join(f"// ==> {path} <==\n{text}\n" for path, text in files.items())


def declarations(ws: portmap.Workspace, path: str) -> tuple[str, int]:
    generator = Generator(ws, path)
    text = generator.generate()
    return text, generator.notes


def declaration_files(ws: portmap.Workspace, path: str) -> tuple[dict[str, str], int]:
    generator = Generator(ws, path)
    files = generator.generate_files()
    return files, generator.notes
