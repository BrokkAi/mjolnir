# `go/crates/chrono` (chrono 0.4.44) and `go/crates/chronotz` (chrono-tz 0.10.4)

Oracle module `go/port/oracle/src/chrono_oracle.rs`, feature `chrono`.

Mjolnir stores and prints timestamps with chrono in about 90 files: RFC 3339
strings in the database, the relay journal and JSON; local times in the
dashboard; durations for quota resets; one `chrono_tz::Tz`. Timestamps cross
between the implementations, so text must match and parsing must accept and
reject the same inputs.

Inventory every chrono item used into `doc.go`, then build over Go's `time`
from chrono's source:

- The types: `DateTime` for `Utc`, `Local`, `FixedOffset` and `Tz` (one
  generic-free `chrono.DateTime` over `time.Time` with typed constructors is
  recommended; document equality and ordering by instant and zone-dependent
  `Debug`/`Display`), `NaiveDate`, `NaiveTime`, `NaiveDateTime`, the
  `TimeZone` methods used with `LocalResult` ambiguity, `Datelike`/`Timelike`,
  `Weekday`, `Month`.
- Output byte for byte: `to_rfc3339`, `to_rfc3339_opts` with every
  `SecondsFormat`, `to_rfc2822`, `Display` and `Debug` of each type, and
  `format()` with every strftime specifier and modifier
  (`format/strftime.rs`, `format/formatting.rs`).
- Parsing with chrono's rules and error text: `parse_from_rfc3339`,
  `parse_from_rfc2822`, `parse_from_str` forms, the `FromStr` impls (more
  lenient than RFC 3339; port `format/parse.rs` and `format/scan.rs`),
  `ParseError` kinds and `Display`, leap seconds.
- Arithmetic: `TimeDelta` (constructors with range checks, `try_` forms,
  accessors, `to_std`/`from_std` with their errors, `abs`, negatives,
  `Display`), checked and panicking add and subtract with chrono's panic text,
  `signed_duration_since`, month and day arithmetic, the year range,
  `with_*`, conversions, timestamps in every unit, `from_timestamp*`, the
  epoch and limits, rounding if used, `Utc::now()` precision, `SystemTime`
  conversion.
- `Local` following chrono's zone rules (`TZ`, `/etc/localtime`) as closely as
  Go allows; document differences.
- serde forms through `go/crates/serde` (read its `doc.go`): chrono's own
  string form and precision, `FromStr` leniency on input, and the
  `ts_seconds`/`ts_milliseconds` modules if used.
- `chronotz`: `Tz` by name and its parse error text, `name()`, `with_timezone`,
  `%Z` abbreviations, over Go's embedded tz database (document the database
  version difference).

Tests: chrono's own tests for what is covered, and differential tests on
thousands of instants (epoch, negative, far bounds, leap days, DST transitions
in several zones, every sub-second precision) for every text form and
accessor; generated and mutated input strings for every parse function; format
strings from every specifier; arithmetic including overflow.
