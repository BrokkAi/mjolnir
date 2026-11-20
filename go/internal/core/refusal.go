// Port of mj-core/src/refusal.rs.

// Refusals: failures whose reason is safe to send to whoever asked.
//
// Most failures name profile homes, project paths, SSH hosts or container
// locators, so they stay on the controller and the caller gets a generic
// answer plus a reference into the daemon log. A refusal is different: it is a
// precondition the caller can fix, and the sentence that says so is written
// for that caller.
//
// The reason travels as a marker on the error chain, so wrapping with
// fmt.Errorf("...: %w") does not hide it:
//
//	return RefusalPrecondition("create a workspace first").Wrap(source)
//
// Anything that is not marked stays internal. That is the safe default: a new
// failure site leaks nothing until someone writes a sentence for it.

package core

import "errors"

// Refusal says why a request was refused, in words meant for the person who
// made it.
type Refusal struct {
	kind    RefusalKind
	message string
	code    *string
	// source is the error this refusal was attached to as context, the way
	// anyhow's `.context(refusal)` keeps the underlying failure on the chain.
	// It is never compared or shown to the caller.
	source error
}

// RefusalKind is which kind of refusal this is. The two map onto the answers
// the HTTP surfaces already give: a state the caller must change first, and a
// request naming something the controller cannot use.
type RefusalKind int

const (
	// RefusalKindPrecondition: something must be set up or finished before
	// this can run.
	RefusalKindPrecondition RefusalKind = iota
	// RefusalKindUnusable: the request itself names something unusable.
	RefusalKindUnusable
)

// RefusalPrecondition is a state the caller has to change before the request
// can run.
func RefusalPrecondition(message string) Refusal {
	return Refusal{kind: RefusalKindPrecondition, message: message}
}

// RefusalUnusable is a request that names something the controller cannot
// use.
func RefusalUnusable(message string) Refusal {
	return Refusal{kind: RefusalKindUnusable, message: message}
}

// WithCode names the reason for clients that choose their own remedy, so the
// message can stay free of instructions for one surface.
func (r Refusal) WithCode(code string) Refusal {
	r.code = &code
	return r
}

// Wrap attaches the refusal as context over source: the result reads as the
// refusal's message and still unwraps to source. This is the Go form of
// anyhow's `source.context(refusal)`.
func (r Refusal) Wrap(source error) error {
	r.source = source
	return r
}

// Code is the reason's code, if it has one.
func (r Refusal) Code() (string, bool) {
	if r.code == nil {
		return "", false
	}
	return *r.code, true
}

func (r Refusal) Kind() RefusalKind { return r.kind }

func (r Refusal) Message() string { return r.message }

// Equal compares kind, message and code, as Rust's PartialEq does.
func (r Refusal) Equal(other Refusal) bool {
	codeA, okA := r.Code()
	codeB, okB := other.Code()
	return r.kind == other.kind && r.message == other.message && okA == okB && codeA == codeB
}

// RefusalOf is the refusal err carries, if any.
//
// The marker is carried by the error, not by its text, and errors.As walks
// every wrapped layer, so added context does not hide it.
func RefusalOf(err error) (Refusal, bool) {
	var refusal Refusal
	if !errors.As(err, &refusal) {
		return Refusal{}, false
	}
	refusal.source = nil
	return refusal, true
}

// Error is the refusal's message.
func (r Refusal) Error() string { return r.message }

// Unwrap is the error the refusal was attached to, if any.
func (r Refusal) Unwrap() error { return r.source }
