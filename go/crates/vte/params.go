// Port of vte 0.15.0 src/params.rs

// Fixed size parameters list with optional subparameters.

package vte

import "github.com/BrokkAi/mjolnir/go/rs"

const paramsMaxParams = 32

// Params is the parameter list of a CSI or DCS sequence: up to 32 parameters
// and subparameters.
type Params struct {
	// Number of subparameters for each parameter.
	//
	// For each entry in the `params` slice, this stores the length of the
	// param as number of subparams at the same index as the param in the
	// `params` slice.
	//
	// At the subparam positions the length will always be `0`.
	subparams [paramsMaxParams]uint8

	// All parameters and subparameters.
	params [paramsMaxParams]uint16

	// Number of suparameters in the current parameter.
	currentSubparams uint8

	// Total number of parameters and subparameters.
	len int
}

// Returns the number of parameters.
func (self *Params) Len() int {
	return self.len
}

// Returns `true` if there are no parameters present.
func (self *Params) IsEmpty() bool {
	return self.len == 0
}

// Returns an iterator over all parameters and subparameters.
func (self *Params) Iter() *ParamsIter {
	return paramsIterNew(self)
}

// Returns `true` if there is no more space for additional parameters.
func (self *Params) isFull() bool {
	return self.len == paramsMaxParams
}

// Clear all parameters.
func (self *Params) clear() {
	self.currentSubparams = 0
	self.len = 0
}

// Add an additional parameter.
func (self *Params) push(item uint16) {
	self.subparams[self.len-int(self.currentSubparams)] = self.currentSubparams + 1
	self.params[self.len] = item
	self.currentSubparams = 0
	self.len += 1
}

// Add an additional subparameter to the current parameter.
func (self *Params) extend(item uint16) {
	self.subparams[self.len-int(self.currentSubparams)] = self.currentSubparams + 1
	self.params[self.len] = item
	self.currentSubparams += 1
	self.len += 1
}

// IntoIter is `impl IntoIterator for &Params`, which is what `for param in
// params` iterates.
func (self *Params) IntoIter() *ParamsIter {
	return self.Iter()
}

// Immutable subparameter iterator.
type ParamsIter struct {
	params *Params
	index  int
}

func paramsIterNew(params *Params) *ParamsIter {
	return &ParamsIter{params: params, index: 0}
}

// Next is `impl Iterator for ParamsIter`: the next parameter with its
// subparameters. The slice points into the Params and is valid as long as
// the Params is not changed, which for a `Perform` method means until it
// returns.
func (self *ParamsIter) Next() rs.Option[[]uint16] {
	if self.index >= self.params.Len() {
		return rs.None[[]uint16]()
	}

	// Get all subparameters for the current parameter.
	numSubparams := self.params.subparams[self.index]
	// The capacity is cut so that appending to a parameter cannot write
	// into the one after it.
	end := self.index + int(numSubparams)
	param := self.params.params[self.index:end:end]

	// Jump to the next parameter.
	self.index += int(numSubparams)

	return rs.Some(param)
}

// SizeHint is Iterator::size_hint.
func (self *ParamsIter) SizeHint() (int, rs.Option[int]) {
	remaining := self.params.Len() - self.index
	return remaining, rs.Some(remaining)
}

// FmtDebug is `impl Debug for Params`: `[1;38:2:255:0:255]`.
func (self *Params) FmtDebug(f *rs.Formatter) error {
	if err := f.WriteStr("["); err != nil {
		return err
	}

	i := 0
	for iter := self.Iter(); ; i++ {
		param, ok := iter.Next().Get()
		if !ok {
			break
		}
		if i != 0 {
			if err := f.WriteStr(";"); err != nil {
				return err
			}
		}

		for i, subparam := range param {
			if i != 0 {
				if err := f.WriteStr(":"); err != nil {
					return err
				}
			}

			if err := rs.DebugFmt(subparam, f); err != nil {
				return err
			}
		}
	}

	return f.WriteStr("]")
}
