// Mjolnir-local stand-ins for the ratatui frame and the few widgets mj-chat
// draws: Frame, Widget, Clear, Block (borders and titles) and Paragraph
// (with wrap and scroll).
//
// They draw into an ultraviolet buffer and must produce the cells ratatui
// produces for Mjolnir's screens, so the copied Rust golden files stay
// unchanged (see "The terminal UI on Charm" in
// .agents/docs/go-port/core-brief.md). Rectangles are uv.Rectangle and
// positions uv.Position.
//
// Declared by chat-2 alongside theme.rs, whose panel and modal hand out
// Blocks; this is the one copy for package chat.
package chat

import (
	"unicode"

	uv "github.com/charmbracelet/ultraviolet"
)

// Widget draws itself into area of buf (ratatui's Widget).
type Widget interface {
	Render(area uv.Rectangle, buf *uv.Buffer)
}

// Frame is what one terminal frame draws into: the buffer and where the
// cursor ends up (ratatui's Frame).
type Frame struct {
	buffer *uv.Buffer
	cursor *uv.Position
}

// NewFrame is a frame drawing into buffer, with the cursor hidden.
func NewFrame(buffer *uv.Buffer) *Frame { return &Frame{buffer: buffer} }

// Area is the whole frame.
func (f *Frame) Area() uv.Rectangle { return f.buffer.Bounds() }

// Buffer is the buffer the frame draws into (Frame::buffer_mut).
func (f *Frame) Buffer() *uv.Buffer { return f.buffer }

// RenderWidget draws widget into area.
func (f *Frame) RenderWidget(widget Widget, area uv.Rectangle) { widget.Render(area, f.buffer) }

// SetCursorPosition shows the cursor at position after the frame is drawn.
func (f *Frame) SetCursorPosition(position uv.Position) { f.cursor = &position }

// CursorPosition is where the frame put the cursor, if it showed it.
func (f *Frame) CursorPosition() (uv.Position, bool) {
	if f.cursor == nil {
		return uv.Position{}, false
	}
	return *f.cursor, true
}

// Clear resets every cell of its area to an empty cell (ratatui's Clear).
type Clear struct{}

func (Clear) Render(area uv.Rectangle, buf *uv.Buffer) {
	buf.ClearArea(area.Intersect(buf.Bounds()))
}

// Borders is ratatui's Borders bit set, in ratatui's bit order.
type Borders uint8

const (
	BordersNone   Borders = 0
	BordersTop    Borders = 1 << 0
	BordersRight  Borders = 1 << 1
	BordersBottom Borders = 1 << 2
	BordersLeft   Borders = 1 << 3
	BordersAll            = BordersTop | BordersRight | BordersBottom | BordersLeft
)

// BorderSet is ratatui's symbols::border::Set: the glyphs of a border.
type BorderSet struct {
	TopLeft          string
	TopRight         string
	BottomLeft       string
	BottomRight      string
	VerticalLeft     string
	VerticalRight    string
	HorizontalTop    string
	HorizontalBottom string
}

// BorderType is ratatui's BorderType, the border sets Mjolnir draws with.
type BorderType uint8

const (
	BorderTypePlain BorderType = iota
	BorderTypeRounded
	BorderTypeDouble
	BorderTypeThick
)

// Set is the glyphs of the border type (BorderType::to_border_set).
func (t BorderType) Set() BorderSet {
	switch t {
	case BorderTypeRounded:
		return BorderSet{"╭", "╮", "╰", "╯", "│", "│", "─", "─"}
	case BorderTypeDouble:
		return BorderSet{"╔", "╗", "╚", "╝", "║", "║", "═", "═"}
	case BorderTypeThick:
		return BorderSet{"┏", "┓", "┗", "┛", "┃", "┃", "━", "━"}
	default:
		return BorderSet{"┌", "┐", "└", "┘", "│", "│", "─", "─"}
	}
}

// Padding is ratatui's Padding: blank cells inside a block's borders.
type Padding struct {
	Left   int
	Right  int
	Top    int
	Bottom int
}

// TitlePosition is ratatui's title Position. The zero value is unset: the
// title follows the block's TitlesPosition.
type TitlePosition uint8

const (
	TitlePositionTop TitlePosition = iota + 1
	TitlePositionBottom
)

// BlockTitle is one title of a Block and the border it sits on.
type BlockTitle struct {
	Position TitlePosition
	Line     Line
}

// Block is ratatui's Block: a bordered box with titles on its top and
// bottom borders. Build one with NewBlock or BorderedBlock; the zero value
// has no border glyphs.
type Block struct {
	Titles          []BlockTitle
	TitlesStyle     Style
	TitlesAlignment Alignment
	TitlesPosition  TitlePosition
	Borders         Borders
	BorderStyle     Style
	BorderSet       BorderSet
	Style           Style
	Padding         Padding
}

// NewBlock is Block::new (and Block::default): no borders, plain glyphs,
// titles on top and left aligned.
func NewBlock() Block {
	return Block{
		TitlesAlignment: AlignmentLeft,
		TitlesPosition:  TitlePositionTop,
		BorderSet:       BorderTypePlain.Set(),
	}
}

// BorderedBlock is Block::bordered.
func BorderedBlock() Block { return NewBlock().WithBorders(BordersAll) }

// WithTitle is Block::title: a title on the block's TitlesPosition.
func (b Block) WithTitle(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Line: title})
	return b
}

// WithTitleTop is Block::title_top.
func (b Block) WithTitleTop(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Position: TitlePositionTop, Line: title})
	return b
}

// WithTitleBottom is Block::title_bottom.
func (b Block) WithTitleBottom(title Line) Block {
	b.Titles = append(append([]BlockTitle(nil), b.Titles...), BlockTitle{Position: TitlePositionBottom, Line: title})
	return b
}

// WithTitleStyle is Block::title_style.
func (b Block) WithTitleStyle(style Style) Block { b.TitlesStyle = style; return b }

// WithTitleAlignment is Block::title_alignment.
func (b Block) WithTitleAlignment(alignment Alignment) Block {
	b.TitlesAlignment = alignment
	return b
}

// WithTitlePosition is Block::title_position.
func (b Block) WithTitlePosition(position TitlePosition) Block {
	b.TitlesPosition = position
	return b
}

// WithBorders is Block::borders.
func (b Block) WithBorders(borders Borders) Block { b.Borders = borders; return b }

// WithBorderStyle is Block::border_style.
func (b Block) WithBorderStyle(style Style) Block { b.BorderStyle = style; return b }

// WithBorderType is Block::border_type.
func (b Block) WithBorderType(borderType BorderType) Block {
	b.BorderSet = borderType.Set()
	return b
}

// WithBorderSet is Block::border_set.
func (b Block) WithBorderSet(set BorderSet) Block { b.BorderSet = set; return b }

// WithStyle is Block::style.
func (b Block) WithStyle(style Style) Block { b.Style = style; return b }

// WithPadding is Block::padding.
func (b Block) WithPadding(padding Padding) Block { b.Padding = padding; return b }

// Inner is the area inside the block's borders, title rows and padding.
func (b Block) Inner(area uv.Rectangle) uv.Rectangle {
	x, y, width, height := area.Min.X, area.Min.Y, area.Dx(), area.Dy()
	if b.Borders&BordersLeft != 0 {
		x = min(x+1, x+width)
		width = max(width-1, 0)
	}
	if b.Borders&BordersTop != 0 || b.hasTitleAt(TitlePositionTop) {
		y = min(y+1, y+height)
		height = max(height-1, 0)
	}
	if b.Borders&BordersRight != 0 {
		width = max(width-1, 0)
	}
	if b.Borders&BordersBottom != 0 || b.hasTitleAt(TitlePositionBottom) {
		height = max(height-1, 0)
	}
	x += b.Padding.Left
	y += b.Padding.Top
	width = max(width-(b.Padding.Left+b.Padding.Right), 0)
	height = max(height-(b.Padding.Top+b.Padding.Bottom), 0)
	return uv.Rect(x, y, width, height)
}

// titlesPosition is the position of titles without their own, Top unless
// the block says otherwise.
func (b Block) titlesPosition() TitlePosition {
	if b.TitlesPosition == 0 {
		return TitlePositionTop
	}
	return b.TitlesPosition
}

func (b Block) titlePosition(title BlockTitle) TitlePosition {
	if title.Position == 0 {
		return b.titlesPosition()
	}
	return title.Position
}

func (b Block) hasTitleAt(position TitlePosition) bool {
	for _, title := range b.Titles {
		if b.titlePosition(title) == position {
			return true
		}
	}
	return false
}

// verticalSpace is the rows the block takes above and below its content
// (ratatui's Block::vertical_space).
func (b Block) verticalSpace() (int, int) {
	top := b.Padding.Top
	if b.Borders&BordersTop != 0 || b.hasTitleAt(TitlePositionTop) {
		top++
	}
	bottom := b.Padding.Bottom
	if b.Borders&BordersBottom != 0 || b.hasTitleAt(TitlePositionBottom) {
		bottom++
	}
	return top, bottom
}

// Render draws the block's style, borders and titles.
func (b Block) Render(area uv.Rectangle, buf *uv.Buffer) {
	area = area.Intersect(buf.Bounds())
	if area.Empty() {
		return
	}
	ratatuiSetStyle(buf, area, b.Style)
	b.renderSides(area, buf)
	b.renderCorners(area, buf)
	for _, position := range []TitlePosition{TitlePositionTop, TitlePositionBottom} {
		// The order of these calls defines how overlapping titles draw.
		b.renderLeftTitles(position, area, buf)
		b.renderCenterTitles(position, area, buf)
		b.renderRightTitles(position, area, buf)
	}
}

func (b Block) renderSides(area uv.Rectangle, buf *uv.Buffer) {
	left, top := area.Min.X, area.Min.Y
	right, bottom := area.Max.X-1, area.Max.Y-1
	sides := []struct {
		border         Borders
		x0, x1, y0, y1 int
		symbol         string
	}{
		{BordersLeft, left, left, top, bottom, b.BorderSet.VerticalLeft},
		{BordersTop, left, right, top, top, b.BorderSet.HorizontalTop},
		{BordersRight, right, right, top, bottom, b.BorderSet.VerticalRight},
		{BordersBottom, left, right, bottom, bottom, b.BorderSet.HorizontalBottom},
	}
	for _, side := range sides {
		if b.Borders&side.border == 0 {
			continue
		}
		for x := side.x0; x <= side.x1; x++ {
			for y := side.y0; y <= side.y1; y++ {
				ratatuiSetSymbol(buf, x, y, side.symbol, b.BorderStyle)
			}
		}
	}
}

func (b Block) renderCorners(area uv.Rectangle, buf *uv.Buffer) {
	corners := []struct {
		borders Borders
		x, y    int
		symbol  string
	}{
		{BordersRight | BordersBottom, area.Max.X - 1, area.Max.Y - 1, b.BorderSet.BottomRight},
		{BordersRight | BordersTop, area.Max.X - 1, area.Min.Y, b.BorderSet.TopRight},
		{BordersLeft | BordersBottom, area.Min.X, area.Max.Y - 1, b.BorderSet.BottomLeft},
		{BordersLeft | BordersTop, area.Min.X, area.Min.Y, b.BorderSet.TopLeft},
	}
	for _, corner := range corners {
		if b.Borders&corner.borders == corner.borders {
			ratatuiSetSymbol(buf, corner.x, corner.y, corner.symbol, b.BorderStyle)
		}
	}
}

// filteredTitles is the titles on position with alignment.
func (b Block) filteredTitles(position TitlePosition, alignment Alignment) []Line {
	titlesAlignment := b.TitlesAlignment
	if titlesAlignment == 0 {
		titlesAlignment = AlignmentLeft
	}
	var titles []Line
	for _, title := range b.Titles {
		if b.titlePosition(title) != position {
			continue
		}
		lineAlignment := title.Line.Alignment
		if lineAlignment == 0 {
			lineAlignment = titlesAlignment
		}
		if lineAlignment == alignment {
			titles = append(titles, title.Line)
		}
	}
	return titles
}

// titlesArea is the row the titles on position draw in, between the side
// borders.
func (b Block) titlesArea(area uv.Rectangle, position TitlePosition) uv.Rectangle {
	leftBorder, rightBorder := 0, 0
	if b.Borders&BordersLeft != 0 {
		leftBorder = 1
	}
	if b.Borders&BordersRight != 0 {
		rightBorder = 1
	}
	y := area.Min.Y
	if position == TitlePositionBottom {
		y = area.Max.Y - 1
	}
	return uv.Rect(area.Min.X+leftBorder, y, max(area.Dx()-leftBorder-rightBorder, 0), 1)
}

func (b Block) renderRightTitles(position TitlePosition, area uv.Rectangle, buf *uv.Buffer) {
	titles := b.filteredTitles(position, AlignmentRight)
	titlesArea := b.titlesArea(area, position)
	// Titles are drawn in reverse order to align them to the right.
	for i := len(titles) - 1; i >= 0; i-- {
		if titlesArea.Empty() {
			break
		}
		titleWidth := titles[i].Width()
		x := max(titlesArea.Max.X-titleWidth, titlesArea.Min.X)
		titleArea := uv.Rect(x, titlesArea.Min.Y, min(titleWidth, titlesArea.Dx()), titlesArea.Dy())
		ratatuiSetStyle(buf, titleArea, b.TitlesStyle)
		titles[i].Render(titleArea, buf)
		// One space between titles.
		titlesArea.Max.X = titlesArea.Min.X + max(titlesArea.Dx()-titleWidth-1, 0)
	}
}

func (b Block) renderCenterTitles(position TitlePosition, area uv.Rectangle, buf *uv.Buffer) {
	area = b.titlesArea(area, position)
	titles := b.filteredTitles(position, AlignmentCenter)
	// Titles are drawn with a space after each one except the last.
	totalWidth := 0
	for _, title := range titles {
		totalWidth += title.Width() + 1
	}
	totalWidth = max(totalWidth-1, 0)
	if totalWidth <= area.Dx() {
		x := area.Min.X + max(area.Dx()-totalWidth, 0)/2
		area.Min.X = x
		for _, title := range titles {
			width := title.Width()
			titleArea := uv.Rect(area.Min.X, area.Min.Y, width, area.Dy())
			ratatuiSetStyle(buf, titleArea, b.TitlesStyle)
			title.Render(titleArea, buf)
			area = indentX(area, width+1)
		}
		return
	}
	// The titles do not fit: the left side is cut by an offset, the right
	// side by the area width.
	offset := max(totalWidth-area.Dx(), 0) / 2
	for _, title := range titles {
		if area.Empty() {
			break
		}
		width := max(min(area.Dx(), title.Width())-offset, 0)
		titleArea := uv.Rect(area.Min.X, area.Min.Y, width, area.Dy())
		ratatuiSetStyle(buf, titleArea, b.TitlesStyle)
		if offset > 0 {
			title.RightAligned().Render(titleArea, buf)
			offset = max(offset-width-1, 0)
		} else {
			title.LeftAligned().Render(titleArea, buf)
		}
		area = indentX(area, width+1)
	}
}

func (b Block) renderLeftTitles(position TitlePosition, area uv.Rectangle, buf *uv.Buffer) {
	titlesArea := b.titlesArea(area, position)
	for _, title := range b.filteredTitles(position, AlignmentLeft) {
		if titlesArea.Empty() {
			break
		}
		titleWidth := title.Width()
		titleArea := uv.Rect(titlesArea.Min.X, titlesArea.Min.Y, min(titleWidth, titlesArea.Dx()), titlesArea.Dy())
		ratatuiSetStyle(buf, titleArea, b.TitlesStyle)
		title.Render(titleArea, buf)
		titlesArea = indentX(titlesArea, titleWidth+1)
	}
}

// ratatuiSetStyle patches style onto every cell of area that lies in buf
// (ratatui's Buffer::set_style). The cells are patched in place, so the
// continuation cells of a wide glyph keep their place.
func ratatuiSetStyle(buf *uv.Buffer, area uv.Rectangle, style Style) {
	area = area.Intersect(buf.Bounds())
	if style == (Style{}) {
		return
	}
	for y := area.Min.Y; y < area.Max.Y; y++ {
		for x := area.Min.X; x < area.Max.X; x++ {
			style.ApplyTo(&buf.CellAt(x, y).Style)
		}
	}
}

// ratatuiSetSymbol is ratatui's `buffer[(x, y)].set_symbol(symbol)
// .set_style(style)`: the cell takes symbol and keeps whatever style does
// not override. A wide symbol makes the cells it covers its continuation,
// which ratatui's writers reset to blanks.
func ratatuiSetSymbol(buf *uv.Buffer, x, y int, symbol string, style Style) {
	current := buf.CellAt(x, y)
	if current == nil {
		return
	}
	cell := *current
	cell.Content = symbol
	// A drawn symbol occupies at least its own cell.
	cell.Width = max(displayWidth(symbol), 1)
	style.ApplyTo(&cell.Style)
	buf.SetCell(x, y, &cell)
}

// ratatuiAppendSymbol appends a zero-width symbol to the cell at (x, y) and
// patches style onto it (ratatui's Cell::append_symbol).
func ratatuiAppendSymbol(buf *uv.Buffer, x, y int, symbol string, style Style) {
	cell := buf.CellAt(x, y)
	if cell == nil {
		return
	}
	cell.Content += symbol
	style.ApplyTo(&cell.Style)
}

// Wrap is ratatui's Wrap: Trim drops leading whitespace of wrapped rows.
type Wrap struct {
	Trim bool
}

// Paragraph is ratatui's Paragraph: text, optionally wrapped and scrolled,
// optionally inside a block.
type Paragraph struct {
	Block     *Block
	Style     Style
	Wrap      *Wrap
	Text      Text
	ScrollY   int
	ScrollX   int
	Alignment Alignment
}

// NewParagraph is Paragraph::new.
func NewParagraph(text Text) Paragraph {
	return Paragraph{Text: text, Alignment: AlignmentLeft}
}

// WithBlock is Paragraph::block.
func (p Paragraph) WithBlock(block Block) Paragraph { p.Block = &block; return p }

// WithStyle is Paragraph::style.
func (p Paragraph) WithStyle(style Style) Paragraph { p.Style = style; return p }

// WithWrap is Paragraph::wrap.
func (p Paragraph) WithWrap(wrap Wrap) Paragraph { p.Wrap = &wrap; return p }

// WithScroll is Paragraph::scroll((y, x)).
func (p Paragraph) WithScroll(y, x int) Paragraph { p.ScrollY, p.ScrollX = y, x; return p }

// WithAlignment is Paragraph::alignment.
func (p Paragraph) WithAlignment(alignment Alignment) Paragraph {
	p.Alignment = alignment
	return p
}

// LineCount is how many rows the paragraph takes at width, block included
// (Paragraph::line_count).
func (p Paragraph) LineCount(width int) int {
	if width < 1 {
		return 0
	}
	top, bottom := 0, 0
	if p.Block != nil {
		top, bottom = p.Block.verticalSpace()
	}
	count := len(p.Text.Lines)
	if p.Wrap != nil {
		composer := newWordWrapper(p.styledLines(p.Style), width, p.Wrap.Trim)
		count = 0
		for {
			if _, _, _, ok := composer.nextLine(); !ok {
				break
			}
			count++
		}
	}
	return count + top + bottom
}

// Render draws the paragraph.
func (p Paragraph) Render(area uv.Rectangle, buf *uv.Buffer) {
	area = area.Intersect(buf.Bounds())
	ratatuiSetStyle(buf, area, p.Style)
	inner := area
	if p.Block != nil {
		p.Block.Render(area, buf)
		inner = p.Block.Inner(area)
	}
	p.renderParagraph(inner, buf)
}

func (p Paragraph) renderParagraph(area uv.Rectangle, buf *uv.Buffer) {
	if area.Empty() {
		return
	}
	ratatuiSetStyle(buf, area, p.Style)
	lines := p.styledLines(p.Text.Style)
	var composer lineComposer
	if p.Wrap != nil {
		wrapper := newWordWrapper(lines, area.Dx(), p.Wrap.Trim)
		// Compute lines until the scroll offset is reached.
		for range p.ScrollY {
			if _, _, _, ok := wrapper.nextLine(); !ok {
				return
			}
		}
		composer = wrapper
	} else {
		// Skip directly to the first visible line.
		lines = lines[min(p.ScrollY, len(lines)):]
		composer = &lineTruncator{lines: lines, maxWidth: area.Dx(), horizontalOffset: p.ScrollX}
	}
	for y := 0; y < area.Dy(); y++ {
		graphemes, width, alignment, ok := composer.nextLine()
		if !ok {
			break
		}
		x := lineOffset(width, area.Dx(), alignment)
		for _, grapheme := range graphemes {
			symbolWidth := displayWidth(grapheme.symbol)
			if symbolWidth == 0 {
				continue
			}
			// Overwrite any previous character with a space rather than a
			// zero-width symbol.
			symbol := grapheme.symbol
			if symbol == "" {
				symbol = " "
			}
			ratatuiSetSymbol(buf, area.Min.X+x, area.Min.Y+y, symbol, grapheme.style)
			x += symbolWidth
		}
	}
}

// lineOffset is where a wrapped line of width starts in an area of
// areaWidth (ratatui's get_line_offset).
func lineOffset(width, areaWidth int, alignment Alignment) int {
	switch alignment {
	case AlignmentCenter:
		return max(areaWidth/2-width/2, 0)
	case AlignmentRight:
		return max(areaWidth-width, 0)
	}
	return 0
}

// styledGrapheme is one grapheme of a paragraph and the style it draws in
// (ratatui's StyledGrapheme).
type styledGrapheme struct {
	symbol string
	style  Style
}

// isWhitespace is ratatui's StyledGrapheme::is_whitespace: a zero-width
// space, or whitespace other than a no-break space.
func (g styledGrapheme) isWhitespace() bool {
	if g.symbol == "​" {
		return true
	}
	if g.symbol == " " {
		return false
	}
	for _, character := range g.symbol {
		if !unicode.IsSpace(character) {
			return false
		}
	}
	return true
}

// styledLine is one line of a paragraph as graphemes, and its alignment.
type styledLine struct {
	graphemes []styledGrapheme
	alignment Alignment
}

// styledLines is the paragraph's lines as styled graphemes over base, each
// with its alignment (Line::styled_graphemes over the paragraph's lines).
func (p Paragraph) styledLines(base Style) []styledLine {
	alignment := p.Alignment
	if alignment == 0 {
		alignment = AlignmentLeft
	}
	lines := make([]styledLine, len(p.Text.Lines))
	for i, line := range p.Text.Lines {
		lineStyle := base.Patch(line.Style)
		var graphemes []styledGrapheme
		for _, span := range line.Spans {
			style := lineStyle.Patch(span.Style)
			for _, grapheme := range splitGraphemes(span.Content) {
				if !hasControl(grapheme.text) {
					graphemes = append(graphemes, styledGrapheme{symbol: grapheme.text, style: style})
				}
			}
		}
		lines[i] = styledLine{graphemes: graphemes, alignment: alignment}
		if line.Alignment != 0 {
			lines[i].alignment = line.Alignment
		}
	}
	return lines
}

// lineComposer packs styled graphemes into rows (ratatui's LineComposer).
type lineComposer interface {
	nextLine() ([]styledGrapheme, int, Alignment, bool)
}

// wordWrapper wraps lines on word boundaries (ratatui's WordWrapper).
type wordWrapper struct {
	lines            []styledLine
	maxWidth         int
	wrapped          [][]styledGrapheme
	currentAlignment Alignment
	trim             bool
}

func newWordWrapper(lines []styledLine, maxWidth int, trim bool) *wordWrapper {
	return &wordWrapper{lines: lines, maxWidth: maxWidth, currentAlignment: AlignmentLeft, trim: trim}
}

// processInput splits one input line into wrapped rows and keeps them to be
// emitted later.
func (w *wordWrapper) processInput(symbols []styledGrapheme) {
	var pendingLine, pendingWord, pendingWhitespace []styledGrapheme
	lineWidth, wordWidth, whitespaceWidth := 0, 0, 0
	nonWhitespacePrevious := false
	for _, grapheme := range symbols {
		isWhitespace := grapheme.isWhitespace()
		symbolWidth := displayWidth(grapheme.symbol)
		// Symbols wider than the line limit are ignored.
		if symbolWidth > w.maxWidth {
			continue
		}
		wordFound := nonWhitespacePrevious && isWhitespace
		// The current word would overflow after removing whitespace.
		trimmedOverflow := len(pendingLine) == 0 && w.trim && wordWidth+symbolWidth > w.maxWidth
		// Separated whitespace would overflow on its own.
		whitespaceOverflow := len(pendingLine) == 0 && w.trim && whitespaceWidth+symbolWidth > w.maxWidth
		// The current full word, whitespace included, would overflow.
		untrimmedOverflow := len(pendingLine) == 0 && !w.trim && wordWidth+whitespaceWidth+symbolWidth > w.maxWidth
		if wordFound || trimmedOverflow || whitespaceOverflow || untrimmedOverflow {
			if len(pendingLine) > 0 || !w.trim {
				pendingLine = append(pendingLine, pendingWhitespace...)
				lineWidth += whitespaceWidth
			}
			pendingLine = append(pendingLine, pendingWord...)
			lineWidth += wordWidth
			pendingWord = pendingWord[:0]
			pendingWhitespace = pendingWhitespace[:0]
			whitespaceWidth = 0
			wordWidth = 0
		}
		lineFull := lineWidth >= w.maxWidth
		pendingWordOverflow := symbolWidth > 0 && lineWidth+whitespaceWidth+wordWidth >= w.maxWidth
		if lineFull || pendingWordOverflow {
			remaining := max(w.maxWidth-lineWidth, 0)
			w.wrapped = append(w.wrapped, pendingLine)
			pendingLine = nil
			lineWidth = 0
			// Whitespace up to the end of the line is dropped.
			for len(pendingWhitespace) > 0 {
				width := displayWidth(pendingWhitespace[0].symbol)
				if width > remaining {
					break
				}
				whitespaceWidth -= width
				remaining -= width
				pendingWhitespace = pendingWhitespace[1:]
			}
			// The first whitespace does not count toward the next word.
			if isWhitespace && len(pendingWhitespace) == 0 {
				continue
			}
		}
		if isWhitespace {
			whitespaceWidth += symbolWidth
			pendingWhitespace = append(pendingWhitespace, grapheme)
		} else {
			wordWidth += symbolWidth
			pendingWord = append(pendingWord, grapheme)
		}
		nonWhitespacePrevious = !isWhitespace
	}
	if len(pendingLine) == 0 && len(pendingWord) == 0 && len(pendingWhitespace) > 0 && w.trim {
		w.wrapped = append(w.wrapped, nil)
	}
	if len(pendingLine) > 0 || !w.trim {
		pendingLine = append(pendingLine, pendingWhitespace...)
	}
	pendingLine = append(pendingLine, pendingWord...)
	if len(pendingLine) > 0 {
		w.wrapped = append(w.wrapped, pendingLine)
	}
	if len(w.wrapped) == 0 {
		w.wrapped = append(w.wrapped, nil)
	}
}

func (w *wordWrapper) nextLine() ([]styledGrapheme, int, Alignment, bool) {
	if w.maxWidth == 0 {
		return nil, 0, 0, false
	}
	for {
		if len(w.wrapped) > 0 {
			line := w.wrapped[0]
			w.wrapped = w.wrapped[1:]
			width := 0
			for _, grapheme := range line {
				width += displayWidth(grapheme.symbol)
			}
			return line, width, w.currentAlignment, true
		}
		if len(w.lines) == 0 {
			return nil, 0, 0, false
		}
		line := w.lines[0]
		w.lines = w.lines[1:]
		w.currentAlignment = line.alignment
		w.processInput(line.graphemes)
	}
}

// lineTruncator cuts lines that overhang the width (ratatui's
// LineTruncator).
type lineTruncator struct {
	lines            []styledLine
	maxWidth         int
	horizontalOffset int
}

func (t *lineTruncator) nextLine() ([]styledGrapheme, int, Alignment, bool) {
	if t.maxWidth == 0 || len(t.lines) == 0 {
		return nil, 0, 0, false
	}
	line := t.lines[0]
	t.lines = t.lines[1:]
	var current []styledGrapheme
	width := 0
	offset := t.horizontalOffset
	for _, grapheme := range line.graphemes {
		symbolWidth := displayWidth(grapheme.symbol)
		// Characters wider than the whole width are ignored.
		if symbolWidth > t.maxWidth {
			continue
		}
		if width+symbolWidth > t.maxWidth {
			break
		}
		symbol := grapheme.symbol
		if offset != 0 && line.alignment == AlignmentLeft {
			if symbolWidth > offset {
				symbol = trimOffset(symbol, offset)
				offset = 0
			} else {
				offset -= symbolWidth
				symbol = ""
			}
		}
		width += displayWidth(symbol)
		current = append(current, styledGrapheme{symbol: symbol, style: grapheme.style})
	}
	return current, width, line.alignment, true
}

// trimOffset drops the leading graphemes of text that fit in offset cells
// (ratatui's trim_offset).
func trimOffset(text string, offset int) string {
	start := 0
	for _, grapheme := range splitGraphemes(text) {
		width := displayWidth(grapheme.text)
		if width > offset {
			break
		}
		offset -= width
		start += len(grapheme.text)
	}
	return text[start:]
}
