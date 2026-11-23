// The linear constraint solver behind Layout.Split: a small Cassowary
// simplex solver following kasuari 0.4 (the solver ratatui 0.30 lays out
// with), and the constraints ratatui-core 0.1's layout feeds it.
//
// ratatui is not ported (see "The terminal UI on Charm" in
// .agents/docs/go-port/core-brief.md); this is the part of it Layout needs
// to place areas on the same cells. It keeps kasuari's arithmetic and its
// pivot rules. kasuari walks hash maps where it chooses among equally good
// pivots; this walks symbols in creation order, so the only results that
// can differ are layouts whose constraints leave several optimal answers.

package chat

import (
	"errors"
	"math"
	"slices"

	uv "github.com/charmbracelet/ultraviolet"
)

// Constraint strengths (kasuari's Strength).
const (
	strengthRequired = 1_001_001_000.0
	strengthStrong   = 1_000_000.0
	strengthMedium   = 1_000.0
	strengthWeak     = 1.0
)

// The strengths ratatui-core's layout gives each kind of constraint.
const (
	layoutSpacerSizeEq    = strengthRequired / 10.0
	layoutMinSizeGe       = strengthStrong * 100.0
	layoutMaxSizeLe       = strengthStrong * 100.0
	layoutLengthSizeEq    = strengthStrong * 10.0
	layoutPercentageEq    = strengthStrong
	layoutRatioSizeEq     = strengthStrong / 10.0
	layoutMinSizeEq       = strengthMedium * 10.0
	layoutMaxSizeEq       = strengthMedium * 10.0
	layoutFillGrow        = strengthMedium
	layoutGrow            = strengthMedium / 10.0
	layoutSpaceGrow       = strengthWeak * 10.0
	layoutAllSegmentGrow  = strengthWeak
	layoutEmptyStrength   = strengthRequired - strengthWeak
	layoutFloatPrecision  = 100.0
	cassowaryNearZeroEpsi = 1e-8
)

type cassowaryRelation uint8

const (
	cassowaryLessOrEqual cassowaryRelation = iota
	cassowaryEqual
	cassowaryGreaterOrEqual
)

// cassowaryTerm is coefficient * variable; variables are small integers.
type cassowaryTerm struct {
	variable    int
	coefficient float64
}

// cassowaryExpression is a sum of terms plus a constant (kasuari's
// Expression). Terms keep their order and may repeat a variable.
type cassowaryExpression struct {
	terms    []cassowaryTerm
	constant float64
}

func cassowaryVariable(variable int) cassowaryExpression {
	return cassowaryExpression{terms: []cassowaryTerm{{variable: variable, coefficient: 1}}}
}

func cassowaryConstant(constant float64) cassowaryExpression {
	return cassowaryExpression{constant: constant}
}

// minus is kasuari's Expression - Expression: the right side's terms are
// appended negated.
func (e cassowaryExpression) minus(other cassowaryExpression) cassowaryExpression {
	terms := slices.Clone(e.terms)
	for _, term := range other.terms {
		terms = append(terms, cassowaryTerm{variable: term.variable, coefficient: -term.coefficient})
	}
	return cassowaryExpression{terms: terms, constant: e.constant - other.constant}
}

func (e cassowaryExpression) times(factor float64) cassowaryExpression {
	terms := slices.Clone(e.terms)
	for i := range terms {
		terms[i].coefficient *= factor
	}
	return cassowaryExpression{terms: terms, constant: e.constant * factor}
}

func (e cassowaryExpression) dividedBy(divisor float64) cassowaryExpression {
	terms := slices.Clone(e.terms)
	for i := range terms {
		terms[i].coefficient /= divisor
	}
	return cassowaryExpression{terms: terms, constant: e.constant / divisor}
}

// cassowaryConstraint is `lhs op rhs`, stored as `lhs - rhs op 0`.
type cassowaryConstraint struct {
	expression cassowaryExpression
	op         cassowaryRelation
	strength   float64
}

func newCassowaryConstraint(lhs cassowaryExpression, op cassowaryRelation, strength float64, rhs cassowaryExpression) cassowaryConstraint {
	return cassowaryConstraint{expression: lhs.minus(rhs), op: op, strength: min(max(strength, 0), strengthRequired)}
}

type cassowarySymbolKind uint8

const (
	cassowaryInvalid cassowarySymbolKind = iota
	cassowaryExternal
	cassowarySlack
	cassowaryError
	cassowaryDummy
)

type cassowarySymbol struct {
	id   int
	kind cassowarySymbolKind
}

func cassowaryNearZero(value float64) bool {
	return math.Abs(value) < cassowaryNearZeroEpsi
}

// cassowaryRow is kasuari's Row: constant + sum of coefficient * symbol.
type cassowaryRow struct {
	cells    map[cassowarySymbol]float64
	constant float64
}

func newCassowaryRow(constant float64) *cassowaryRow {
	return &cassowaryRow{cells: map[cassowarySymbol]float64{}, constant: constant}
}

func (r *cassowaryRow) clone() *cassowaryRow {
	clone := newCassowaryRow(r.constant)
	for symbol, value := range r.cells {
		clone.cells[symbol] = value
	}
	return clone
}

// symbols is the row's symbols in creation order, the order this solver
// chooses among them.
func (r *cassowaryRow) symbols() []cassowarySymbol {
	symbols := make([]cassowarySymbol, 0, len(r.cells))
	for symbol := range r.cells {
		symbols = append(symbols, symbol)
	}
	slices.SortFunc(symbols, func(a, b cassowarySymbol) int { return a.id - b.id })
	return symbols
}

func (r *cassowaryRow) insertSymbol(symbol cassowarySymbol, coefficient float64) {
	value, ok := r.cells[symbol]
	if !ok {
		if !cassowaryNearZero(coefficient) {
			r.cells[symbol] = coefficient
		}
		return
	}
	value += coefficient
	if cassowaryNearZero(value) {
		delete(r.cells, symbol)
	} else {
		r.cells[symbol] = value
	}
}

func (r *cassowaryRow) insertRow(other *cassowaryRow, coefficient float64) {
	r.constant += other.constant * coefficient
	for _, symbol := range other.symbols() {
		r.insertSymbol(symbol, other.cells[symbol]*coefficient)
	}
}

func (r *cassowaryRow) reverseSign() {
	r.constant = -r.constant
	for symbol, value := range r.cells {
		r.cells[symbol] = -value
	}
}

func (r *cassowaryRow) solveForSymbol(symbol cassowarySymbol) {
	coefficient := -1.0 / r.cells[symbol]
	delete(r.cells, symbol)
	r.constant *= coefficient
	for other, value := range r.cells {
		r.cells[other] = value * coefficient
	}
}

func (r *cassowaryRow) solveForSymbols(lhs, rhs cassowarySymbol) {
	r.insertSymbol(lhs, -1.0)
	r.solveForSymbol(rhs)
}

func (r *cassowaryRow) coefficientFor(symbol cassowarySymbol) float64 {
	return r.cells[symbol]
}

func (r *cassowaryRow) substitute(symbol cassowarySymbol, row *cassowaryRow) {
	if coefficient, ok := r.cells[symbol]; ok {
		delete(r.cells, symbol)
		r.insertRow(row, coefficient)
	}
}

type cassowaryTag struct {
	marker cassowarySymbol
	other  cassowarySymbol
}

// cassowarySolver is kasuari's Solver without edit variables, which layout
// never uses.
type cassowarySolver struct {
	varSymbols map[int]cassowarySymbol
	rows       map[cassowarySymbol]*cassowaryRow
	objective  *cassowaryRow
	artificial *cassowaryRow
	idTick     int
}

var (
	errCassowaryUnsatisfiable = errors.New("the constraint can not be satisfied")
	errCassowaryUnbounded     = errors.New("the objective is unbounded")
)

func newCassowarySolver() *cassowarySolver {
	return &cassowarySolver{
		varSymbols: map[int]cassowarySymbol{},
		rows:       map[cassowarySymbol]*cassowaryRow{},
		objective:  newCassowaryRow(0),
		idTick:     1,
	}
}

func (s *cassowarySolver) rowSymbols() []cassowarySymbol {
	symbols := make([]cassowarySymbol, 0, len(s.rows))
	for symbol := range s.rows {
		symbols = append(symbols, symbol)
	}
	slices.SortFunc(symbols, func(a, b cassowarySymbol) int { return a.id - b.id })
	return symbols
}

func (s *cassowarySolver) newSymbol(kind cassowarySymbolKind) cassowarySymbol {
	symbol := cassowarySymbol{id: s.idTick, kind: kind}
	s.idTick++
	return symbol
}

func (s *cassowarySolver) varSymbol(variable int) cassowarySymbol {
	if symbol, ok := s.varSymbols[variable]; ok {
		return symbol
	}
	symbol := s.newSymbol(cassowaryExternal)
	s.varSymbols[variable] = symbol
	return symbol
}

func (s *cassowarySolver) addConstraint(constraint cassowaryConstraint) error {
	row, tag := s.createRow(constraint)
	subject := chooseCassowarySubject(row, tag)
	if subject.kind == cassowaryInvalid && cassowaryAllDummies(row) {
		if !cassowaryNearZero(row.constant) {
			return errCassowaryUnsatisfiable
		}
		subject = tag.marker
	}
	if subject.kind == cassowaryInvalid {
		satisfiable, err := s.addWithArtificialVariable(row)
		if err != nil {
			return err
		}
		if !satisfiable {
			return errCassowaryUnsatisfiable
		}
	} else {
		row.solveForSymbol(subject)
		s.substitute(subject, row)
		s.rows[subject] = row
	}
	return s.optimize(s.objective)
}

func (s *cassowarySolver) createRow(constraint cassowaryConstraint) (*cassowaryRow, cassowaryTag) {
	expression := constraint.expression
	row := newCassowaryRow(expression.constant)
	for _, term := range expression.terms {
		if cassowaryNearZero(term.coefficient) {
			continue
		}
		symbol := s.varSymbol(term.variable)
		if other, ok := s.rows[symbol]; ok {
			row.insertRow(other, term.coefficient)
		} else {
			row.insertSymbol(symbol, term.coefficient)
		}
	}
	var tag cassowaryTag
	switch constraint.op {
	case cassowaryLessOrEqual, cassowaryGreaterOrEqual:
		coefficient := -1.0
		if constraint.op == cassowaryLessOrEqual {
			coefficient = 1.0
		}
		slack := s.newSymbol(cassowarySlack)
		row.insertSymbol(slack, coefficient)
		tag = cassowaryTag{marker: slack}
		if constraint.strength < strengthRequired {
			errorSymbol := s.newSymbol(cassowaryError)
			row.insertSymbol(errorSymbol, -coefficient)
			s.objective.insertSymbol(errorSymbol, constraint.strength)
			tag.other = errorSymbol
		}
	case cassowaryEqual:
		if constraint.strength < strengthRequired {
			errPlus := s.newSymbol(cassowaryError)
			errMinus := s.newSymbol(cassowaryError)
			row.insertSymbol(errPlus, -1.0)
			row.insertSymbol(errMinus, 1.0)
			s.objective.insertSymbol(errPlus, constraint.strength)
			s.objective.insertSymbol(errMinus, constraint.strength)
			tag = cassowaryTag{marker: errPlus, other: errMinus}
		} else {
			dummy := s.newSymbol(cassowaryDummy)
			row.insertSymbol(dummy, 1.0)
			tag = cassowaryTag{marker: dummy}
		}
	}
	if row.constant < 0 {
		row.reverseSign()
	}
	return row, tag
}

func chooseCassowarySubject(row *cassowaryRow, tag cassowaryTag) cassowarySymbol {
	for _, symbol := range row.symbols() {
		if symbol.kind == cassowaryExternal {
			return symbol
		}
	}
	if (tag.marker.kind == cassowarySlack || tag.marker.kind == cassowaryError) && row.coefficientFor(tag.marker) < 0 {
		return tag.marker
	}
	if (tag.other.kind == cassowarySlack || tag.other.kind == cassowaryError) && row.coefficientFor(tag.other) < 0 {
		return tag.other
	}
	return cassowarySymbol{}
}

func cassowaryAllDummies(row *cassowaryRow) bool {
	for symbol := range row.cells {
		if symbol.kind != cassowaryDummy {
			return false
		}
	}
	return true
}

func (s *cassowarySolver) addWithArtificialVariable(row *cassowaryRow) (bool, error) {
	art := s.newSymbol(cassowarySlack)
	s.rows[art] = row.clone()
	s.artificial = row.clone()
	if err := s.optimize(s.artificial); err != nil {
		return false, err
	}
	success := cassowaryNearZero(s.artificial.constant)
	s.artificial = nil
	if artRow, ok := s.rows[art]; ok {
		delete(s.rows, art)
		if len(artRow.cells) == 0 {
			return success, nil
		}
		entering := cassowaryAnyPivotableSymbol(artRow)
		if entering.kind == cassowaryInvalid {
			return false, nil
		}
		artRow.solveForSymbols(art, entering)
		s.substitute(entering, artRow)
		s.rows[entering] = artRow
	}
	for _, other := range s.rows {
		delete(other.cells, art)
	}
	delete(s.objective.cells, art)
	return success, nil
}

func (s *cassowarySolver) substitute(symbol cassowarySymbol, row *cassowaryRow) {
	for _, other := range s.rowSymbols() {
		s.rows[other].substitute(symbol, row)
	}
	s.objective.substitute(symbol, row)
	if s.artificial != nil {
		s.artificial.substitute(symbol, row)
	}
}

func (s *cassowarySolver) optimize(objective *cassowaryRow) error {
	for {
		entering := cassowaryEnteringSymbol(objective)
		if entering.kind == cassowaryInvalid {
			return nil
		}
		leaving, row, ok := s.leavingRow(entering)
		if !ok {
			return errCassowaryUnbounded
		}
		row.solveForSymbols(leaving, entering)
		s.substitute(entering, row)
		s.rows[entering] = row
	}
}

func cassowaryEnteringSymbol(objective *cassowaryRow) cassowarySymbol {
	for _, symbol := range objective.symbols() {
		if symbol.kind != cassowaryDummy && objective.cells[symbol] < 0 {
			return symbol
		}
	}
	return cassowarySymbol{}
}

func cassowaryAnyPivotableSymbol(row *cassowaryRow) cassowarySymbol {
	for _, symbol := range row.symbols() {
		if symbol.kind == cassowarySlack || symbol.kind == cassowaryError {
			return symbol
		}
	}
	return cassowarySymbol{}
}

func (s *cassowarySolver) leavingRow(entering cassowarySymbol) (cassowarySymbol, *cassowaryRow, bool) {
	ratio := math.Inf(1)
	var found cassowarySymbol
	ok := false
	for _, symbol := range s.rowSymbols() {
		if symbol.kind == cassowaryExternal {
			continue
		}
		row := s.rows[symbol]
		temp := row.coefficientFor(entering)
		if temp < 0 {
			tempRatio := -row.constant / temp
			if tempRatio < ratio {
				ratio = tempRatio
				found = symbol
				ok = true
			}
		}
	}
	if !ok {
		return cassowarySymbol{}, nil, false
	}
	row := s.rows[found]
	delete(s.rows, found)
	return found, row, true
}

// value is the variable's solved value; a variable that is not basic is 0.
func (s *cassowarySolver) value(variable int) float64 {
	symbol, ok := s.varSymbols[variable]
	if !ok {
		return 0
	}
	if row, ok := s.rows[symbol]; ok {
		return row.constant
	}
	return 0
}

// layoutElement is a span between two solver variables: a segment or a
// spacer.
type layoutElement struct {
	start int
	end   int
}

func (e layoutElement) size() cassowaryExpression {
	return cassowaryVariable(e.end).minus(cassowaryVariable(e.start))
}

func (e layoutElement) hasMaxSize(size int, strength float64) cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryLessOrEqual, strength, cassowaryConstant(float64(size)*layoutFloatPrecision))
}

func (e layoutElement) hasMinSize(size int, strength float64) cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryGreaterOrEqual, strength, cassowaryConstant(float64(size)*layoutFloatPrecision))
}

func (e layoutElement) hasIntSize(size int, strength float64) cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryEqual, strength, cassowaryConstant(float64(size)*layoutFloatPrecision))
}

func (e layoutElement) hasSize(size cassowaryExpression, strength float64) cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryEqual, strength, size)
}

func (e layoutElement) hasDoubleSize(size cassowaryExpression, strength float64) cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryEqual, strength, size.times(2.0))
}

func (e layoutElement) isEmpty() cassowaryConstraint {
	return newCassowaryConstraint(e.size(), cassowaryEqual, layoutEmptyStrength, cassowaryConstant(0))
}

// solveLayout is ratatui-core's Layout::try_split for the segments.
func solveLayout(layout Layout, area uv.Rectangle) ([]uv.Rectangle, error) {
	solver := newCassowarySolver()
	inner := RectInner(area, layout.Margin)
	var areaStart, areaEnd float64
	if layout.Direction == DirectionHorizontal {
		areaStart = float64(inner.Min.X) * layoutFloatPrecision
		areaEnd = float64(inner.Max.X) * layoutFloatPrecision
	} else {
		areaStart = float64(inner.Min.Y) * layoutFloatPrecision
		areaEnd = float64(inner.Max.Y) * layoutFloatPrecision
	}

	constraints := layout.Constraints
	variableCount := len(constraints)*2 + 2
	spacers := make([]layoutElement, 0, len(constraints)+1)
	for v := 0; v+1 < variableCount; v += 2 {
		spacers = append(spacers, layoutElement{start: v, end: v + 1})
	}
	segments := make([]layoutElement, 0, len(constraints))
	for v := 1; v+1 < variableCount; v += 2 {
		segments = append(segments, layoutElement{start: v, end: v + 1})
	}
	flex := layout.Flex
	spacing := layout.Spacing
	areaSize := layoutElement{start: 0, end: variableCount - 1}

	add := func(constraint cassowaryConstraint) error { return solver.addConstraint(constraint) }
	var all []cassowaryConstraint

	// configure_area
	all = append(all,
		newCassowaryConstraint(cassowaryVariable(areaSize.start), cassowaryEqual, strengthRequired, cassowaryConstant(areaStart)),
		newCassowaryConstraint(cassowaryVariable(areaSize.end), cassowaryEqual, strengthRequired, cassowaryConstant(areaEnd)),
	)
	// configure_variable_in_area_constraints
	for v := range variableCount {
		all = append(all,
			newCassowaryConstraint(cassowaryVariable(v), cassowaryGreaterOrEqual, strengthRequired, cassowaryVariable(areaSize.start)),
			newCassowaryConstraint(cassowaryVariable(v), cassowaryLessOrEqual, strengthRequired, cassowaryVariable(areaSize.end)),
		)
	}
	// configure_variable_constraints
	for v := 1; v+1 < variableCount; v += 2 {
		all = append(all, newCassowaryConstraint(cassowaryVariable(v), cassowaryLessOrEqual, strengthRequired, cassowaryVariable(v+1)))
	}
	all = append(all, layoutFlexConstraints(areaSize, spacers, flex, spacing)...)
	// configure_constraints
	for i, constraint := range constraints {
		segment := segments[i]
		switch constraint := constraint.(type) {
		case ConstraintMax:
			all = append(all,
				segment.hasMaxSize(int(constraint), layoutMaxSizeLe),
				segment.hasIntSize(int(constraint), layoutMaxSizeEq),
			)
		case ConstraintMin:
			all = append(all, segment.hasMinSize(int(constraint), layoutMinSizeGe))
			if flex == FlexLegacy {
				all = append(all, segment.hasIntSize(int(constraint), layoutMinSizeEq))
			} else {
				all = append(all, segment.hasSize(areaSize.size(), layoutFillGrow))
			}
		case ConstraintLength:
			all = append(all, segment.hasIntSize(int(constraint), layoutLengthSizeEq))
		case ConstraintPercentage:
			size := areaSize.size().times(float64(constraint)).dividedBy(100.0)
			all = append(all, segment.hasSize(size, layoutPercentageEq))
		case ConstraintRatio:
			size := areaSize.size().times(float64(constraint.Num)).dividedBy(float64(max(constraint.Den, 1)))
			all = append(all, segment.hasSize(size, layoutRatioSizeEq))
		case ConstraintFill:
			all = append(all, segment.hasSize(areaSize.size(), layoutFillGrow))
		}
	}
	// configure_fill_constraints
	type scaled struct {
		segment layoutElement
		scale   float64
	}
	var growing []scaled
	for i, constraint := range constraints {
		switch constraint := constraint.(type) {
		case ConstraintFill:
			growing = append(growing, scaled{segment: segments[i], scale: max(float64(constraint), 1e-6)})
		case ConstraintMin:
			if flex != FlexLegacy {
				growing = append(growing, scaled{segment: segments[i], scale: 1.0})
			}
		}
	}
	for i := range growing {
		for j := i + 1; j < len(growing); j++ {
			left, right := growing[i], growing[j]
			all = append(all, newCassowaryConstraint(
				left.segment.size().times(right.scale), cassowaryEqual, layoutGrow, right.segment.size().times(left.scale),
			))
		}
	}
	if flex != FlexLegacy {
		for i := 0; i+1 < len(segments); i++ {
			all = append(all, segments[i].hasSize(segments[i+1].size(), layoutAllSegmentGrow))
		}
	}
	for _, constraint := range all {
		if err := add(constraint); err != nil {
			return nil, err
		}
	}

	rects := make([]uv.Rectangle, 0, len(segments))
	for _, segment := range segments {
		start := layoutRoundToCell(solver.value(segment.start))
		end := layoutRoundToCell(solver.value(segment.end))
		size := max(end-start, 0)
		if layout.Direction == DirectionHorizontal {
			rects = append(rects, uv.Rect(start, inner.Min.Y, size, inner.Dy()))
		} else {
			rects = append(rects, uv.Rect(inner.Min.X, start, inner.Dx(), size))
		}
	}
	return rects, nil
}

// layoutRoundToCell converts a solved position back to a cell, as
// `round(round(v) / 100) as u16` does (negative positions saturate to 0).
func layoutRoundToCell(value float64) int {
	cell := math.Round(math.Round(value) / layoutFloatPrecision)
	return int(min(max(cell, 0), math.MaxUint16))
}

// layoutFlexConstraints is ratatui-core's configure_flex_constraints.
func layoutFlexConstraints(area layoutElement, spacers []layoutElement, flex Flex, spacing int) []cassowaryConstraint {
	var inner []layoutElement
	if len(spacers) >= 2 {
		inner = spacers[1 : len(spacers)-1]
	}
	spacingF64 := float64(spacing) * layoutFloatPrecision
	var all []cassowaryConstraint
	pairs := func(elements []layoutElement, each func(left, right layoutElement)) {
		for i := range elements {
			for j := i + 1; j < len(elements); j++ {
				each(elements[i], elements[j])
			}
		}
	}
	first, last := spacers[0], spacers[len(spacers)-1]
	switch flex {
	case FlexLegacy:
		for _, spacer := range inner {
			all = append(all, spacer.hasSize(cassowaryConstant(spacingF64), layoutSpacerSizeEq))
		}
		all = append(all, first.isEmpty(), last.isEmpty())
	case FlexSpaceAround:
		if len(spacers) <= 2 {
			pairs(spacers, func(left, right layoutElement) {
				all = append(all, left.hasSize(right.size(), layoutSpacerSizeEq))
			})
			for _, spacer := range spacers {
				all = append(all, spacer.hasMinSize(spacing, layoutSpacerSizeEq), spacer.hasSize(area.size(), layoutSpaceGrow))
			}
		} else {
			middle := spacers[1 : len(spacers)-1]
			pairs(middle, func(left, right layoutElement) {
				all = append(all, left.hasSize(right.size(), layoutSpacerSizeEq))
			})
			if len(middle) > 0 {
				all = append(all,
					middle[0].hasDoubleSize(first.size(), layoutSpacerSizeEq),
					middle[0].hasDoubleSize(last.size(), layoutSpacerSizeEq),
				)
			}
			for _, spacer := range spacers {
				all = append(all, spacer.hasMinSize(spacing, layoutSpacerSizeEq), spacer.hasSize(area.size(), layoutSpaceGrow))
			}
		}
	case FlexSpaceEvenly:
		pairs(spacers, func(left, right layoutElement) {
			all = append(all, left.hasSize(right.size(), layoutSpacerSizeEq))
		})
		for _, spacer := range spacers {
			all = append(all, spacer.hasMinSize(spacing, layoutSpacerSizeEq), spacer.hasSize(area.size(), layoutSpaceGrow))
		}
	case FlexSpaceBetween:
		pairs(inner, func(left, right layoutElement) {
			all = append(all, left.hasSize(right.size(), layoutSpacerSizeEq))
		})
		for _, spacer := range inner {
			all = append(all, spacer.hasMinSize(spacing, layoutSpacerSizeEq), spacer.hasSize(area.size(), layoutSpaceGrow))
		}
		all = append(all, first.isEmpty(), last.isEmpty())
	case FlexStart:
		for _, spacer := range inner {
			all = append(all, spacer.hasSize(cassowaryConstant(spacingF64), layoutSpacerSizeEq))
		}
		all = append(all, first.isEmpty(), last.hasSize(area.size(), layoutGrow))
	case FlexCenter:
		for _, spacer := range inner {
			all = append(all, spacer.hasSize(cassowaryConstant(spacingF64), layoutSpacerSizeEq))
		}
		all = append(all,
			first.hasSize(area.size(), layoutGrow),
			last.hasSize(area.size(), layoutGrow),
			first.hasSize(last.size(), layoutSpacerSizeEq),
		)
	case FlexEnd:
		for _, spacer := range inner {
			all = append(all, spacer.hasSize(cassowaryConstant(spacingF64), layoutSpacerSizeEq))
		}
		all = append(all, last.isEmpty(), first.hasSize(area.size(), layoutGrow))
	}
	return all
}
