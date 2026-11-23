// Port of mj-core/src/test_log.rs.

// A log handler for tests that have to prove what was logged, and at which
// level.

package core

import (
	"context"
	"fmt"
	"log/slog"
	"strconv"
	"strings"
	"sync"
)

// CapturedLogEvent is one logged event: its level and its fields written
// out, message included.
type CapturedLogEvent struct {
	Level slog.Level
	Text  string
}

// CapturedLog records every event logged through it, as its level and its
// fields written out, message included (Rust: a tracing subscriber).
//
// Install it with slog.New(log), as the default logger or as the logger
// handed to the code under test. The zero value is not usable; build one
// with NewCapturedLog. Copies made by WithAttrs and WithGroup record into
// the same events.
type CapturedLog struct {
	events *capturedLogEvents
	attrs  []slog.Attr
	group  string
}

type capturedLogEvents struct {
	mu     sync.Mutex
	events []CapturedLogEvent
}

func NewCapturedLog() *CapturedLog {
	return &CapturedLog{events: &capturedLogEvents{}}
}

// AtOrAbove is the events at level or more severe.
func (l *CapturedLog) AtOrAbove(level slog.Level) []string {
	var texts []string
	for _, event := range l.Events() {
		if event.Level >= level {
			texts = append(texts, event.Text)
		}
	}
	return texts
}

// At is the events at exactly level.
func (l *CapturedLog) At(level slog.Level) []string {
	var texts []string
	for _, event := range l.Events() {
		if event.Level == level {
			texts = append(texts, event.Text)
		}
	}
	return texts
}

func (l *CapturedLog) Events() []CapturedLogEvent {
	l.events.mu.Lock()
	defer l.events.mu.Unlock()
	return append([]CapturedLogEvent(nil), l.events.events...)
}

func (l *CapturedLog) Enabled(context.Context, slog.Level) bool { return true }

// Handle writes the message and every field as ` name=value`, in the order
// they were given. The message and values formatted with %s are written as
// they are; other strings are quoted, as Rust's Debug writes them.
func (l *CapturedLog) Handle(_ context.Context, record slog.Record) error {
	var text strings.Builder
	fmt.Fprintf(&text, " message=%s", record.Message)
	write := func(attr slog.Attr) {
		key := attr.Key
		if l.group != "" {
			key = l.group + "." + key
		}
		value := attr.Value.Resolve()
		if value.Kind() == slog.KindString {
			fmt.Fprintf(&text, " %s=%s", key, strconv.Quote(value.String()))
		} else {
			fmt.Fprintf(&text, " %s=%v", key, value.Any())
		}
	}
	for _, attr := range l.attrs {
		write(attr)
	}
	record.Attrs(func(attr slog.Attr) bool {
		write(attr)
		return true
	})
	l.events.mu.Lock()
	l.events.events = append(l.events.events, CapturedLogEvent{Level: record.Level, Text: text.String()})
	l.events.mu.Unlock()
	return nil
}

func (l *CapturedLog) WithAttrs(attrs []slog.Attr) slog.Handler {
	copied := *l
	copied.attrs = append(append([]slog.Attr(nil), l.attrs...), attrs...)
	return &copied
}

func (l *CapturedLog) WithGroup(name string) slog.Handler {
	copied := *l
	if copied.group != "" {
		name = copied.group + "." + name
	}
	copied.group = name
	return &copied
}
