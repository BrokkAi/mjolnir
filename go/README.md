# mjolnir-go

This directory holds a Go implementation of Mjolnir that is being built beside
the Rust one. It is a translation, not a rewrite: the Go `mj` and `mj-worker`
are meant to accept the same commands, print the same text, speak the same
protocols, and read and write the same files and database as the Rust
programs, so the two can be run side by side and compared.

It is not finished. This command says exactly how far it is:

```bash
python3 go/port/status.py
```

## Layout

| Path | What it is |
| --- | --- |
| `go.mod`, `assets.go` (repository root) | the Go module, and the package that embeds the same asset files the Rust build compiles in |
| `go/rs/` | Rust's standard library as Go packages: `Option`, formatting, collections, paths, files, locks, threads |
| `go/crates/<name>/` | one package for each third-party Rust crate Mjolnir uses, with the crate's names and behaviour |
| `go/mj*/` | one package for each Mjolnir library crate (`mj-core` is `go/mjcore`) |
| `go/cmd/mj`, `go/cmd/mj-worker` | the programs |
| `go/port/` | the tools that drive the port: the Rust indexer, the name and schedule map, the status report, and the oracle |

Every Go file under `go/mj*` and `go/cmd` is the port of one Rust file and
names it in its first line.

## Working on it

The plan is `.agents/plans/port-mjolnir-to-go.md`. The rules for translating a
Rust file are `.agents/docs/go-port/translation-rules.md`, and the contract for
the library packages is `.agents/docs/go-port/runtime-packages.md`.

```bash
go build ./go/...                                   # build
go/port/gotest.sh ./go/...                          # test, with the right tags and environment
python3 go/port/portmap.py file mj-core/src/hex.rs  # what one Rust file becomes
python3 go/port/status.py --drift                   # Rust files changed since they were ported
MJ_ORACLE=live go/port/gotest.sh ./go/rs/...        # compare with the real Rust libraries
```

A Go build is a development build. Run it only against a named instance, never
your default one:

```bash
target/go/release/mj --instance go-demo
```
