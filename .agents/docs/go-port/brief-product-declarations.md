# Brief: the declarations pass for one crate

You are porting one crate of Mjolnir from Rust to Go. The message that sent
you here names the crate. This is the first of two passes: you make every
type, constant and function signature of the crate exist in Go and compile,
with function bodies that panic with `rs.Todo`. The second pass, by other
people working in parallel on separate files, fills the bodies in. They can
only do that if every name and signature they need is already there and right,
so the measure of your work is that nobody has to change a declaration later.

## Read first

1. `.agents/docs/go-port/translation-rules.md`, all of it. Rules 2 (names),
   3 (types), 9 (serialization) and 10 (conditional compilation) are yours.
2. `.agents/docs/go-port/runtime-packages.md` ("Rules for every runtime
   package") and the `doc.go` of each runtime package the crate uses, so you
   know what the Go names of library types are. `go/rs/registry.go` for the
   registration calls. `.agents/docs/go-port/acp-mapping.md` if the crate uses
   the Agent Client Protocol types. `.agents/docs/go-port/counted-arcs.md` for
   the types whose `Arc` must be `rs.Arc`.
3. The crate's `src/lib.rs` (or `main.rs`) and a skim of its modules.

Do not start agents or forks of your own. The coordinator decides how many
units run at once (the harness allows twenty in total, helpers included), and
every one of them is paid for by the owner.

## Steps

Run everything from the repository root.

1. Generate. `python3 go/port/portmap.py files <crate>` lists the crate's
   production Rust files. Write their declarations:

       python3 go/port/portmap.py declarations --write $(python3 go/port/portmap.py files <crate>)
       gofmt -w go/<package>/

   The generator never overwrites an existing Go file.
2. Make it compile for every platform:

       go build -tags 'mj_debug mj_test_support mj_test_hooks mj_test_identity mj_golden mj_github_history' ./go/<package>/
       go vet   -tags '...same...' ./go/<package>/
       GOOS=darwin  go build ./go/<package>/
       GOOS=windows go build ./go/<package>/
       go build ./go/<package>/        # no tags: the release shape

   What that takes:
   - Every `PORT:` comment is a place the generator could not decide. Settle
     each from the Rust source and remove the comment. A type marked `any /*
     PORT: ... */` gets its real Go type.
   - Every `PORT_VALUE` is a constant or static whose value the generator
     could not derive. Translate the Rust initializer shown above it. A
     `LazyLock` or `OnceLock` initializer is a small closure: translate it, or
     if it calls functions whose bodies are not written yet, leave the call in
     place (it will panic with `rs.Todo` when first used, which is correct for
     now).
   - Items the Rust source has twice under opposite `cfg`s were written to
     separate build-tagged files. Check each split against the Rust `cfg`
     (rule 10), and that each platform still has exactly one definition.
   - `macro_rules!` definitions have no Go counterpart. Leave the comment
     naming each one; the people writing bodies expand it at each use.
   - Traits with associated types, generic traits, and methods with their own
     type parameters need a Go shape the generator only sketches. Decide it
     (rule 3), write it, and explain it in a comment on the type.
   - Hand-written `Serialize`/`Deserialize` impls take the method signatures
     `go/crates/serde/doc.go` defines.
3. Check every signature against its `// rust:` line, not only the ones that
   fail to compile: the receiver (value for `Copy` types, pointer otherwise),
   each parameter (a pointer for `&mut T` and for `&T` of a struct; a value
   for `&str`, `&[T]`, `&Path` and small `Copy` types), the results (`Result<T,
   E>` is `(T, error)`, `Result<()>` is `error`, a tuple is several results),
   `ctx` first if and only if the Rust function is `async`, and type
   parameters. A wrong signature that happens to compile costs far more later
   than now.
4. Check every serialized type against its Rust declaration: field order,
   every `serde` attribute on the container, on each field and on each
   variant, the variant list and each variant's shape. These decide the bytes
   on the wire.
5. Do not write function bodies. A body stays `panic(rs.Todo("<rust path>"))`
   exactly as generated, so that `python3 go/port/status.py` counts it. The
   exceptions are values of constants and statics, and the one-line forwarding
   methods the generator wrote.
6. Do not change a generated name. If one is wrong, that is a defect in the
   generator: say so in your report (with the Rust item and what you
   expected), and work around it only if it blocks compiling.
7. Do not write tests in this pass, and do not commit.

## Report

Lead with the state: does `go build` pass for linux, darwin and windows, with
and without the tags; how many files, and how many `rs.Todo` bodies they hold
(`python3 go/port/status.py --todo <crate>`). Then, briefly:

- every decision you made that the rules did not dictate (trait shapes,
  generic methods, a type you had to change), so the people writing bodies
  find it;
- every defect or gap in the generator, as "Rust item, what it wrote, what is
  right", so it can be fixed before the next crate;
- every runtime package name you needed that does not exist, or exists under
  another name;
- anything in the Rust declarations that looked wrong.

Under 500 words. The declarations themselves are the detail.
