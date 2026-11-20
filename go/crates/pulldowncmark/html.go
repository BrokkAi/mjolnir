// Port of pulldown-cmark 0.10.3 src/html.rs

// Copyright 2015 Google Inc. All rights reserved.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

package pulldowncmark

import (
	"io"
	"strconv"
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// HTML renderer that takes an iterator of events as input.
//
// This is the crate's `html` module (its "html" feature). Mjolnir builds the
// crate without that feature; the module is ported because the crate's
// specification tests compare rendered HTML.

// HtmlEventIterator is the bound `I: Iterator<Item = Event<'a>>` of the HTML
// functions: anything with Rust's `next`, such as a *Parser.
type HtmlEventIterator interface {
	Next() rs.Option[Event]
}

type htmlTableState uint8

const (
	htmlTableState_Head htmlTableState = iota
	htmlTableState_Body
)

type htmlHtmlWriter struct {
	// Iterator supplying events.
	iter HtmlEventIterator

	// Writer to write to.
	writer escapeStrWrite

	// Whether or not the last write wrote a newline.
	endNewline bool

	// Whether if inside a metadata block (text should not be written)
	inNonWritingBlock bool

	tableState      htmlTableState
	tableAlignments []Alignment
	tableCellIndex  int
	numbers         map[string]int
}

func htmlHtmlWriterNew(iter HtmlEventIterator, writer escapeStrWrite) htmlHtmlWriter {
	return htmlHtmlWriter{
		iter:              iter,
		writer:            writer,
		endNewline:        true,
		inNonWritingBlock: false,
		tableState:        htmlTableState_Head,
		tableAlignments:   []Alignment{},
		tableCellIndex:    0,
		numbers:           map[string]int{},
	}
}

// writeNewline writes a new line.
func (w *htmlHtmlWriter) writeNewline() error {
	w.endNewline = true
	return w.writer.WriteStr("\n")
}

// write writes a buffer, and tracks whether or not a newline was written.
func (w *htmlHtmlWriter) write(s string) error {
	if err := w.writer.WriteStr(s); err != nil {
		return err
	}

	if len(s) != 0 {
		w.endNewline = strings.HasSuffix(s, "\n")
	}
	return nil
}

// numberFor is `*self.numbers.entry(name).or_insert(len)`.
func (w *htmlHtmlWriter) numberFor(name string, len_ int) int {
	if number, ok := w.numbers[name]; ok {
		return number
	}
	w.numbers[name] = len_
	return len_
}

func (w *htmlHtmlWriter) run() error {
	for {
		event, ok := w.iter.Next().Get()
		if !ok {
			break
		}
		switch event := event.(type) {
		case *Event_Start:
			if err := w.startTag(event.F0); err != nil {
				return err
			}
		case *Event_End:
			if err := w.endTag(event.F0); err != nil {
				return err
			}
		case *Event_Text:
			text := event.F0
			if !w.inNonWritingBlock {
				if err := escapeEscapeHtmlBodyText(w.writer, text); err != nil {
					return err
				}
				w.endNewline = strings.HasSuffix(text, "\n")
			}
		case *Event_Code:
			text := event.F0
			if err := w.write("<code>"); err != nil {
				return err
			}
			if err := escapeEscapeHtmlBodyText(w.writer, text); err != nil {
				return err
			}
			if err := w.write("</code>"); err != nil {
				return err
			}
		case *Event_Html:
			if err := w.write(event.F0); err != nil {
				return err
			}
		case *Event_InlineHtml:
			if err := w.write(event.F0); err != nil {
				return err
			}
		case *Event_SoftBreak:
			if err := w.writeNewline(); err != nil {
				return err
			}
		case *Event_HardBreak:
			if err := w.write("<br />\n"); err != nil {
				return err
			}
		case *Event_Rule:
			if w.endNewline {
				if err := w.write("<hr />\n"); err != nil {
					return err
				}
			} else {
				if err := w.write("\n<hr />\n"); err != nil {
					return err
				}
			}
		case *Event_FootnoteReference:
			name := event.F0
			len_ := len(w.numbers) + 1
			if err := w.write("<sup class=\"footnote-reference\"><a href=\"#"); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, name); err != nil {
				return err
			}
			if err := w.write("\">"); err != nil {
				return err
			}
			number := w.numberFor(name, len_)
			if err := w.writer.WriteStr(strconv.Itoa(number)); err != nil {
				return err
			}
			if err := w.write("</a></sup>"); err != nil {
				return err
			}
		case *Event_TaskListMarker:
			if event.F0 {
				if err := w.write("<input disabled=\"\" type=\"checkbox\" checked=\"\"/>\n"); err != nil {
					return err
				}
			} else {
				if err := w.write("<input disabled=\"\" type=\"checkbox\"/>\n"); err != nil {
					return err
				}
			}
		default:
			panic(rs.Unreachable(event))
		}
	}
	return nil
}

// startTag writes the start of an HTML tag.
func (w *htmlHtmlWriter) startTag(tag Tag) error {
	switch tag := tag.(type) {
	case *Tag_HtmlBlock:
		return nil
	case *Tag_Paragraph:
		if w.endNewline {
			return w.write("<p>")
		} else {
			return w.write("\n<p>")
		}
	case *Tag_Heading:
		level, id, classes, attrs := tag.Level, tag.Id, tag.Classes, tag.Attrs
		if w.endNewline {
			w.endNewline = false
			if err := w.write("<"); err != nil {
				return err
			}
		} else {
			if err := w.write("\n<"); err != nil {
				return err
			}
		}
		if err := w.writer.WriteStr(level.String()); err != nil {
			return err
		}
		if id, ok := id.Get(); ok {
			if err := w.write(" id=\""); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, id); err != nil {
				return err
			}
			if err := w.write("\""); err != nil {
				return err
			}
		}
		if len(classes) > 0 {
			class := classes[0]
			if err := w.write(" class=\""); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, class); err != nil {
				return err
			}
			for _, class := range classes[1:] {
				if err := w.write(" "); err != nil {
					return err
				}
				if err := escapeEscapeHtml(w.writer, class); err != nil {
					return err
				}
			}
			if err := w.write("\""); err != nil {
				return err
			}
		}
		for i := range attrs {
			attr, value := attrs[i].F0, attrs[i].F1
			if err := w.write(" "); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, attr); err != nil {
				return err
			}
			if val, ok := value.Get(); ok {
				if err := w.write("=\""); err != nil {
					return err
				}
				if err := escapeEscapeHtml(w.writer, val); err != nil {
					return err
				}
				if err := w.write("\""); err != nil {
					return err
				}
			} else {
				if err := w.write("=\"\""); err != nil {
					return err
				}
			}
		}
		return w.write(">")
	case *Tag_Table:
		w.tableAlignments = tag.F0
		return w.write("<table>")
	case *Tag_TableHead:
		w.tableState = htmlTableState_Head
		w.tableCellIndex = 0
		return w.write("<thead><tr>")
	case *Tag_TableRow:
		w.tableCellIndex = 0
		return w.write("<tr>")
	case *Tag_TableCell:
		switch w.tableState {
		case htmlTableState_Head:
			if err := w.write("<th"); err != nil {
				return err
			}
		case htmlTableState_Body:
			if err := w.write("<td"); err != nil {
				return err
			}
		}
		alignment := Alignment_None
		if w.tableCellIndex < len(w.tableAlignments) {
			alignment = w.tableAlignments[w.tableCellIndex]
		}
		switch alignment {
		case Alignment_Left:
			return w.write(" style=\"text-align: left\">")
		case Alignment_Center:
			return w.write(" style=\"text-align: center\">")
		case Alignment_Right:
			return w.write(" style=\"text-align: right\">")
		default:
			return w.write(">")
		}
	case *Tag_BlockQuote:
		if w.endNewline {
			return w.write("<blockquote>\n")
		} else {
			return w.write("\n<blockquote>\n")
		}
	case *Tag_CodeBlock:
		if !w.endNewline {
			if err := w.writeNewline(); err != nil {
				return err
			}
		}
		switch info := tag.F0.(type) {
		case *CodeBlockKind_Fenced:
			// info.split(' ').next().unwrap()
			lang, _, _ := strings.Cut(info.F0, " ")
			if len(lang) == 0 {
				return w.write("<pre><code>")
			} else {
				if err := w.write("<pre><code class=\"language-"); err != nil {
					return err
				}
				if err := escapeEscapeHtml(w.writer, lang); err != nil {
					return err
				}
				return w.write("\">")
			}
		case *CodeBlockKind_Indented:
			return w.write("<pre><code>")
		default:
			panic(rs.Unreachable(info))
		}
	case *Tag_List:
		start, ordered := tag.F0.Get()
		switch {
		case ordered && start == 1:
			if w.endNewline {
				return w.write("<ol>\n")
			} else {
				return w.write("\n<ol>\n")
			}
		case ordered:
			if w.endNewline {
				if err := w.write("<ol start=\""); err != nil {
					return err
				}
			} else {
				if err := w.write("\n<ol start=\""); err != nil {
					return err
				}
			}
			if err := w.writer.WriteStr(strconv.FormatUint(start, 10)); err != nil {
				return err
			}
			return w.write("\">\n")
		default:
			if w.endNewline {
				return w.write("<ul>\n")
			} else {
				return w.write("\n<ul>\n")
			}
		}
	case *Tag_Item:
		if w.endNewline {
			return w.write("<li>")
		} else {
			return w.write("\n<li>")
		}
	case *Tag_Emphasis:
		return w.write("<em>")
	case *Tag_Strong:
		return w.write("<strong>")
	case *Tag_Strikethrough:
		return w.write("<del>")
	case *Tag_Link:
		destUrl, title := tag.DestUrl, tag.Title
		if tag.LinkType == LinkType_Email {
			if err := w.write("<a href=\"mailto:"); err != nil {
				return err
			}
		} else {
			if err := w.write("<a href=\""); err != nil {
				return err
			}
		}
		if err := escapeEscapeHref(w.writer, destUrl); err != nil {
			return err
		}
		if len(title) != 0 {
			if err := w.write("\" title=\""); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, title); err != nil {
				return err
			}
		}
		return w.write("\">")
	case *Tag_Image:
		destUrl, title := tag.DestUrl, tag.Title
		if err := w.write("<img src=\""); err != nil {
			return err
		}
		if err := escapeEscapeHref(w.writer, destUrl); err != nil {
			return err
		}
		if err := w.write("\" alt=\""); err != nil {
			return err
		}
		if err := w.rawText(); err != nil {
			return err
		}
		if len(title) != 0 {
			if err := w.write("\" title=\""); err != nil {
				return err
			}
			if err := escapeEscapeHtml(w.writer, title); err != nil {
				return err
			}
		}
		return w.write("\" />")
	case *Tag_FootnoteDefinition:
		name := tag.F0
		if w.endNewline {
			if err := w.write("<div class=\"footnote-definition\" id=\""); err != nil {
				return err
			}
		} else {
			if err := w.write("\n<div class=\"footnote-definition\" id=\""); err != nil {
				return err
			}
		}
		if err := escapeEscapeHtml(w.writer, name); err != nil {
			return err
		}
		if err := w.write("\"><sup class=\"footnote-definition-label\">"); err != nil {
			return err
		}
		len_ := len(w.numbers) + 1
		number := w.numberFor(name, len_)
		if err := w.writer.WriteStr(strconv.Itoa(number)); err != nil {
			return err
		}
		return w.write("</sup>")
	case *Tag_MetadataBlock:
		w.inNonWritingBlock = true
		return nil
	default:
		panic(rs.Unreachable(tag))
	}
}

func (w *htmlHtmlWriter) endTag(tag TagEnd) error {
	switch tag := tag.(type) {
	case *TagEnd_HtmlBlock:
	case *TagEnd_Paragraph:
		if err := w.write("</p>\n"); err != nil {
			return err
		}
	case *TagEnd_Heading:
		level := tag.F0
		if err := w.write("</"); err != nil {
			return err
		}
		if err := w.writer.WriteStr(level.String()); err != nil {
			return err
		}
		if err := w.write(">\n"); err != nil {
			return err
		}
	case *TagEnd_Table:
		if err := w.write("</tbody></table>\n"); err != nil {
			return err
		}
	case *TagEnd_TableHead:
		if err := w.write("</tr></thead><tbody>\n"); err != nil {
			return err
		}
		w.tableState = htmlTableState_Body
	case *TagEnd_TableRow:
		if err := w.write("</tr>\n"); err != nil {
			return err
		}
	case *TagEnd_TableCell:
		switch w.tableState {
		case htmlTableState_Head:
			if err := w.write("</th>"); err != nil {
				return err
			}
		case htmlTableState_Body:
			if err := w.write("</td>"); err != nil {
				return err
			}
		}
		w.tableCellIndex += 1
	case *TagEnd_BlockQuote:
		if err := w.write("</blockquote>\n"); err != nil {
			return err
		}
	case *TagEnd_CodeBlock:
		if err := w.write("</code></pre>\n"); err != nil {
			return err
		}
	case *TagEnd_List:
		if tag.F0 {
			if err := w.write("</ol>\n"); err != nil {
				return err
			}
		} else {
			if err := w.write("</ul>\n"); err != nil {
				return err
			}
		}
	case *TagEnd_Item:
		if err := w.write("</li>\n"); err != nil {
			return err
		}
	case *TagEnd_Emphasis:
		if err := w.write("</em>"); err != nil {
			return err
		}
	case *TagEnd_Strong:
		if err := w.write("</strong>"); err != nil {
			return err
		}
	case *TagEnd_Strikethrough:
		if err := w.write("</del>"); err != nil {
			return err
		}
	case *TagEnd_Link:
		if err := w.write("</a>"); err != nil {
			return err
		}
	case *TagEnd_Image: // shouldn't happen, handled in start
	case *TagEnd_FootnoteDefinition:
		if err := w.write("</div>\n"); err != nil {
			return err
		}
	case *TagEnd_MetadataBlock:
		w.inNonWritingBlock = false
	default:
		panic(rs.Unreachable(tag))
	}
	return nil
}

// run raw text, consuming end tag
func (w *htmlHtmlWriter) rawText() error {
	nest := 0
loop:
	for {
		event, ok := w.iter.Next().Get()
		if !ok {
			break
		}
		switch event := event.(type) {
		case *Event_Start:
			nest += 1
		case *Event_End:
			if nest == 0 {
				break loop
			}
			nest -= 1
		case *Event_Html:
		case *Event_InlineHtml:
			if err := w.rawTextText(event.F0); err != nil {
				return err
			}
		case *Event_Code:
			if err := w.rawTextText(event.F0); err != nil {
				return err
			}
		case *Event_Text:
			if err := w.rawTextText(event.F0); err != nil {
				return err
			}
		case *Event_SoftBreak, *Event_HardBreak, *Event_Rule:
			if err := w.write(" "); err != nil {
				return err
			}
		case *Event_FootnoteReference:
			name := event.F0
			len_ := len(w.numbers) + 1
			number := w.numberFor(name, len_)
			if err := w.writer.WriteStr("[" + strconv.Itoa(number) + "]"); err != nil {
				return err
			}
		case *Event_TaskListMarker:
			if event.F0 {
				if err := w.write("[x]"); err != nil {
					return err
				}
			} else {
				if err := w.write("[ ]"); err != nil {
					return err
				}
			}
		default:
			panic(rs.Unreachable(event))
		}
	}
	return nil
}

// rawTextText is the arm `InlineHtml(text) | Code(text) | Text(text)` of
// raw_text.
func (w *htmlHtmlWriter) rawTextText(text string) error {
	// Don't use escape_html_body_text here.
	// The output of this function is used in the `alt` attribute.
	if err := escapeEscapeHtml(w.writer, text); err != nil {
		return err
	}
	w.endNewline = strings.HasSuffix(text, "\n")
	return nil
}

// HtmlPushHtml iterates over an `Iterator` of `Event`s, generates HTML for
// each `Event`, and pushes it to a `String`.
//
// The Rust function appends to a `&mut String`; here s is the string to
// append to.
func HtmlPushHtml(s *string, iter HtmlEventIterator) {
	var out strings.Builder
	writer := htmlHtmlWriterNew(iter, escapeStringWriter{F0: &out})
	// Rust appends to the string as it goes, so what was written before a
	// panic in the iterator stays in it.
	defer func() { *s += out.String() }()
	rs.UnwrapUnit(writer.run())
}

// HtmlWriteHtml iterates over an `Iterator` of `Event`s, generates HTML for
// each `Event`, and writes it out to a writable stream.
//
// **Note**: using this function with an unbuffered writer like a file or socket
// will result in poor performance. Wrap these in a buffered writer to prevent
// unnecessary slowdowns.
func HtmlWriteHtml(writer io.Writer, iter HtmlEventIterator) error {
	w := htmlHtmlWriterNew(iter, escapeWriteWrapper{F0: writer})
	return w.run()
}
