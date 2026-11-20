// Port of anyhow 1.0.102 tests/test_chain.rs

package anyhow

import "testing"

// chainError is `anyhow!({ 0 }).context(1).context(2).context(3)`: a message
// and contexts that are numbers, not strings.
func chainError() error {
	return Context(Context(Context(Msg(int32(0)), int32(1)), int32(2)), int32(3))
}

func TestItTestChain_test_iter(t *testing.T) {
	e := chainError()
	chain := Chain(e)
	assertEq(t, "3", chain[0].Error())
	assertEq(t, "2", chain[1].Error())
	assertEq(t, "1", chain[2].Error())
	assertEq(t, "0", chain[3].Error())
	assertEq(t, 4, len(chain))
}

func TestItTestChain_test_rev(t *testing.T) {
	e := chainError()
	chain := Chain(e)
	// `e.chain().rev()`: the slice from its end.
	assertEq(t, "0", chain[len(chain)-1].Error())
	assertEq(t, "1", chain[len(chain)-2].Error())
	assertEq(t, "2", chain[len(chain)-3].Error())
	assertEq(t, "3", chain[len(chain)-4].Error())
	assertEq(t, 4, len(chain))
}

func TestItTestChain_test_len(t *testing.T) {
	e := chainError()
	chain := Chain(e)
	next := func() error {
		head := chain[0]
		chain = chain[1:]
		return head
	}
	nextBack := func() error {
		last := chain[len(chain)-1]
		chain = chain[:len(chain)-1]
		return last
	}
	assertEq(t, 4, len(chain))
	assertEq(t, "3", next().Error())
	assertEq(t, 3, len(chain))
	assertEq(t, "0", nextBack().Error())
	assertEq(t, 2, len(chain))
	assertEq(t, "2", next().Error())
	assertEq(t, 1, len(chain))
	assertEq(t, "1", nextBack().Error())
	assertEq(t, 0, len(chain))
}

func TestItTestChain_test_default(t *testing.T) {
	// `Chain::default()` is the empty chain.
	assertEq(t, 0, len(Chain(nil)))
}

func TestItTestChain_test_clone(t *testing.T) {
	e := chainError()
	chain := append([]error(nil), Chain(e)...)
	assertEq(t, "3", chain[0].Error())
	assertEq(t, "2", chain[1].Error())
	assertEq(t, "1", chain[2].Error())
	assertEq(t, "0", chain[3].Error())
	assertEq(t, 4, len(chain))
}
