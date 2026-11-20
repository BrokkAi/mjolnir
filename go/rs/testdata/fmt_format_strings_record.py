#!/usr/bin/env python3
"""Records what rustc says about format strings, for TestFmtFormatStringsAsRustc.

A format string is checked when rustc compiles a `format!` call, so the oracle
(compiled once) cannot be asked about one it has not seen. This script writes
one Rust program for each pair of a format string and an argument list,
compiles and runs them with the rustc on PATH (it must be 1.99.1, the release
go/rs ports), and writes fmt_format_strings.json.gz beside itself: a JSON list
of [format, arguments, accepted, output], where output is what the program
printed, or the first error line when rustc refused the call.

    python3 go/rs/testdata/fmt_format_strings_record.py
"""
import concurrent.futures
import gzip
import json
import os
import subprocess
import sys
import tempfile

FORMATS = [
    "", "x", "{}", "{} {}", "{0}", "{1}", "{2}", "{0} {1}", "{1} {0}", "{} {0}", "{0} {}", "{} {} {}",
    "{a}", "{b}", "{a} {b}", "{a} {}", "{} {a}", "{c}", "{_}", "{_a}", "{a_}", "{a1}", "{1a}", "{a b}", "{a,b}",
    "{", "}", "{{", "}}", "{{}", "{}}", "{{}}", "{{{}}}", "{{{{}}}}", "{ {", "} }", "}{", "{}{", "{}}{{",
    "{ }", "{  }", "{\t}", "{\n}", "{ 0}", "{0 }", "{ 0 }", "{0 :x}", "{0: x}", "{0:x }", "{ :x}", "{: }", "{:x }", "{ a }", "{a }", "{ a}",
    "{:}", "{0:}", "{a:}", "{:?}", "{:#?}", "{:x}", "{:X}", "{:o}", "{:b}", "{:e}", "{:E}", "{:p}", "{:x?}", "{:X?}", "{:#x?}", "{:?x}", "{:?#}", "{:?X}",
    "{:y}", "{:xx}", "{:x x}", "{:xy}", "{:??}", "{:d}", "{:s}", "{:i}", "{:u}", "{:f}", "{:g}", "{:c}", "{:_}", "{:é}", "{:ab}", "{:x1}", "{:e1}", "{:1e}", "{:1x1}",
    "{:5}", "{:05}", "{:005}", "{:0}", "{:00}", "{:000}", "{:<5}", "{:>5}", "{:^5}", "{:<}", "{:>}", "{:^}", "{:<<}", "{:><5}", "{:<>5}", "{:^^5}",
    "{:*<5}", "{:*5}", "{:**<5}", "{:}<5}", "{:{<5}", "{:}}<5}", "{:0<5}", "{:1<5}", "{:5<5}", "{:5<}", "{:+<5}", "{:#<5}", "{:.<5}", "{:$<5}", "{::<5}", "{: <5}", "{:é<5}", "{:\t<5}", "{:\n<5}", "{:x<5}", "{:?<5}",
    "{:+}", "{:-}", "{:+-}", "{:-+}", "{:++}", "{:+5}", "{:5+}", "{:#}", "{:##}", "{:#+}", "{:+#}", "{:+#05}", "{:#+05}", "{:0#5}", "{:0+5}", "{:+05}", "{:-05}", "{:<+#05}", "{:+<5}",
    "{:.}", "{:.0}", "{:.5}", "{:5.}", "{:5.5}", "{:.5.5}", "{:..5}", "{:.*}", "{:5.*}", "{:.*x}", "{:*}", "{:.**}", "{0:.*}", "{1:.*}", "{a:.*}", "{:.*} {}", "{} {:.*}", "{:.*} {:.*}", "{0:.*} {1}",
    "{:1$}", "{:0$}", "{:00$}", "{:01$}", "{:2$}", "{:1$.0$}", "{:.1$}", "{:.0$}", "{:1$.1$}", "{:a$}", "{:.a$}", "{:a$.b$}", "{:b$.a$}", "{:c$}", "{:.c$}", "{:$}", "{:1$$}", "{:a$$}", "{:.$}", "{:a$x}", "{:.a$x}", "{:a$.b$x}", "{:1 $}", "{:a $}", "{: 1$}",
    "{0:1$}", "{1:0$}", "{0:0$}", "{a:b$}", "{a:a$}", "{b:.a$}", "{0:a$}", "{a:0$}", "{a:1$}", "{0:.1$} {a:b$}", "{} {} {a} {b}", "{0} {1} {a} {b}", "{0} {1} {2} {3}", "{} {} {} {}", "{3}", "{4}", "{2:x} {3:x} {0} {1}",
    "{:65535}", "{:65536}", "{:.65535}", "{:.65536}", "{65535}", "{65536}", "{:99999999999999999999}", "{:65535$}", "{:65536$}", "{:.65536$}",
    "{:x$}", "{:e$}", "{:x?$}", "{x}", "{e}", "{:.x$}",
    "{r#a}", "{r}", "{:#}", "{:#x}", "{:#X}", "{:#b}", "{:#o}", "{:#e}", "{:#p}", "{:08.3}", "{:+08.3e}", "{:^+#08.3E}",
    "{:5?}", "{:<5?}", "{:.3?}", "{:#5?}", "{:#05x?}", "{:5x?}", "{:.3x?}", "{:x ?}", "{:x? }", "{:5 }", "{:5 x}",
    "{0:}", "{0:?}", "{a:?}", "{a:#?}", "{a:x}", "{a:5}", "{a:<5}", "{a:.3}", "{a:b$.0$}",
    "{}{}", "{}{}{}", "{0}{0}", "{0}{0}{1}", "{a}{a}", "{a}{0}", "{a}{2}", "{b}{3}", "{2}{a}",
    "{:?}{:x}{:X}{:o}", "é{}é", "{}\n", "\t{}", "{:\u00a0<5}", "{\u00a0}", "{\u2003}", "{:x\u2003}", "{:\u3000^5}",
    "{a.b}", "{a[0]}", "{a()}", "{0.0}", "{a:?:?}", "{:,}", "{a,}", "{:1.2.3}", "{:1$2$}", "{:>+#010.3x?}", "{:#?#}", "a}b", "a{b", "{}}}", "{{}}}", "{{{}}", "{{{}",
    "{:<+}", "{:+<}", "{:+^5}", "{:^+5}", "{:#^5}", "{:^#5}", "{:0^5}", "{:^05}", "{:^005}", "{:5^}", "{:<5<}", "{:<5>}",
    "{0:0}", "{0:00}", "{0:0$}", "{0:00$}", "{1:0$}", "{0:1}", "{0:01}", "{0:10$}", "{0:1$.0$}", "{1:1$.1$}",
    "{:.0$x}", "{:.1$?}", "{:0$?}", "{:0$x?}", "{:a$?}", "{:a$x?}", "{:.a$X?}", "{:#a$x?}", "{:#0a$x}", "{:0a$}", "{:00a$}", "{:+0a$}",
    "{a:a$.a$}", "{0:0$.0$}", "{:0$.0$}", "{:.0$} {:.0$}", "{:.*} {0}", "{:.*} {1}", "{1} {:.*}", "{a} {:.*}", "{:.*} {a}", "{:.*}{:.*}{}",
    "{é}", "{:é$}", "{aé}", "{a\u0301}", "{\u0301}", "{a-b}", "{a+b}", "{-}", "{+}", "{#}", "{?}", "{.}", "{$}", "{*}", "{0$}", "{a$}",
    "{:}}", "{{:}}", "{{:x}}", "{{{:x}}}", "{{{{}}", "}}}}", "{{{{", "{}{}{", "}{}",
    "{0x1}", "{0b1}", "{01}", "{00}", "{000}", "{1_0}", "{+0}", "{-0}", "{0.}", "{.0}",
    "{:x?}{:X?}", "{:x?} {:?}", "{:#x?}{:#X?}", "{:e}{:E}", "{:.2e}", "{:5e}", "{:05e}", "{:+e}", "{:#e}",
    "{:b}{:o}", "{:#b}", "{:08b}", "{:#010b}", "{:#o}", "{:<#8o}|", "{:^8x}|", "{:>+8}|", "{:<08}|", "{:é>8}|", "{:é>08}|",
]

ARGS = ["", "5usize", "5usize, 7usize", "5usize, a = 2usize", "a = 2usize", "a = 2usize, b = 3usize", "5usize, 7usize, a = 2usize, b = 3usize"]


def rust_literal(s):
    out = '"'
    for ch in s:
        if ch == '"':
            out += '\\"'
        elif ch == '\\':
            out += '\\\\'
        elif ch == '\n':
            out += '\\n'
        elif ch == '\t':
            out += '\\t'
        elif ord(ch) < 0x20 or ord(ch) > 0x7e:
            out += '\\u{%x}' % ord(ch)
        else:
            out += ch
    return out + '"'


def run(work, index, job):
    fmt, args = job
    src = os.path.join(work, f"case{index}.rs")
    exe = os.path.join(work, f"case{index}")
    call = "format!(" + rust_literal(fmt) + (", " + args if args else "") + ")"
    with open(src, "w", encoding="utf-8") as f:
        f.write("#![allow(warnings)]\nfn main() { print!(\"{}\", " + call + "); }\n")
    proc = subprocess.run(["rustc", "--edition", "2024", "-C", "opt-level=0", "-C", "debuginfo=0", "-o", exe, src],
                          capture_output=True, text=True)
    if proc.returncode != 0:
        first = ""
        for line in proc.stderr.splitlines():
            if line.startswith("error"):
                first = line
                break
        return [fmt, args, False, first]
    out = subprocess.run([exe], capture_output=True)
    if out.returncode != 0:
        return [fmt, args, False, "runtime: " + out.stderr.decode("utf-8", "replace").strip().splitlines()[1]]
    return [fmt, args, True, out.stdout.decode("utf-8")]


def main():
    version = subprocess.run(["rustc", "--version"], capture_output=True, text=True, check=True).stdout
    if not version.startswith("rustc 1.99.1 "):
        sys.exit("rustc 1.99.1 is required, found " + version.strip())
    jobs = [(f, a) for f in FORMATS for a in ARGS]
    with tempfile.TemporaryDirectory() as work:
        with concurrent.futures.ThreadPoolExecutor(max_workers=os.cpu_count()) as pool:
            results = list(pool.map(lambda pair: run(work, *pair), enumerate(jobs)))
    text = json.dumps(results, ensure_ascii=False, indent=0) + "\n"
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fmt_format_strings.json.gz")
    with open(out, "wb") as f:
        # mtime=0 so that recording the same results gives the same file.
        f.write(gzip.compress(text.encode("utf-8"), compresslevel=9, mtime=0))
    print(len(results), "cases,", sum(1 for r in results if r[2]), "accepted")


if __name__ == "__main__":
    main()
