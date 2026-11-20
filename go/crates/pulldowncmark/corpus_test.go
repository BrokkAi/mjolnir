package pulldowncmark

import (
	"os"
	"strconv"
	"strings"
	"testing"
)

// The generated corpus of the differential tests: Markdown documents built
// to land on the places where two CommonMark parsers come apart. Generation
// is deterministic; the same documents come out on every run and every
// machine.

// corpusRand is a SplitMix64 generator. The standard library's generators
// are not used so that no Go release can change the corpus.
type corpusRand struct{ state uint64 }

func (r *corpusRand) next() uint64 {
	r.state += 0x9e3779b97f4a7c15
	z := r.state
	z = (z ^ (z >> 30)) * 0xbf58476d1ce4e5b9
	z = (z ^ (z >> 27)) * 0x94d049bb133111eb
	return z ^ (z >> 31)
}

// intn returns a number in [0, n).
func (r *corpusRand) intn(n int) int { return int(r.next() % uint64(n)) }

// between returns a number in [low, high].
func (r *corpusRand) between(low int, high int) int { return low + r.intn(high-low+1) }

// chance is true percent times in a hundred.
func (r *corpusRand) chance(percent int) bool { return r.intn(100) < percent }

func (r *corpusRand) pick(items []string) string { return items[r.intn(len(items))] }

func (r *corpusRand) pickInt(items []int) int { return items[r.intn(len(items))] }

// corpusTokens is the alphabet of the token soups: every delimiter, marker
// and escape the parser gives a meaning to, the whitespace it tells apart,
// and a few words. A token listed twice is drawn twice as often.
var corpusTokens = []string{
	// emphasis and strikethrough delimiter runs
	"*", "*", "*", "**", "**", "***", "****", "_", "_", "__", "___", "~", "~", "~~", "~~", "~~~",
	// code spans and fences
	"`", "`", "`", "``", "``", "```", "````", "~~~~",
	// links, images, references, footnotes
	"[", "[", "[", "]", "]", "]", "(", ")", "![", "](", "][", "[]", "]:", "]: ", "[^", "[^a]", "[a]", "[b]", "[A]",
	"[a]:", "[^a]:", "(/u)", "(/u \"t\")", "(<u v>)", "(u 't')", "(u (t))", "((u))", "</u>", "\"t\"", "'t'", "(t)",
	// html
	"<", "<", ">", "<a>", "</a>", "<b c=\"d\">", "<b c='d' e=f g>", "<div>", "</div>", "<!--", "-->", "<!-->", "<?", "?>",
	"<!X", "<![CDATA[", "]]>", "<pre>", "</pre>", "<script>", "</script>", "<style>", "<textarea>", "<http://a.b>",
	"<a@b.c>", "<x-y z='1'/>", "<br/>", "<p", "<td>", "<a\n", "<a b=\n",
	// escapes and entities
	"\\", "\\", "\\\\", "\\*", "\\_", "\\`", "\\[", "\\]", "\\|", "\\<", "\\&", "\\#", "\\~", "\\\n", "&", "&amp;",
	"&#42;", "&#x2A;", "&#0;", "&#xD800;", "&#1234567;", "&bogus;", "&copy", "&nbsp;", "&ffi;", ";",
	// block markers
	"#", "##", "# ", "###### ", "#######", " #", "-", "- ", "- ", "+ ", "* ", "1. ", "1) ", "2. ", "10. ", "123456789. ",
	"1234567890. ", ">", "> ", "> ", "=", "==", "===", "---", "---", "- - -", "***", "___", "* * *", ":", "|", "|", "|",
	" | ", "|-|", "-|-", ":-:", "|:-|-:|", "---|---", "+++", "...", "[ ]", "[x]", "[X] ", "[ ] ",
	// whitespace and line endings
	" ", " ", " ", " ", "  ", "  ", "   ", "    ", "     ", "\t", "\t", "\n", "\n", "\n", "\n", "\n", "\n\n", "\n\n",
	"\r\n", "\r\n", "\r", "  \n", "\\\n", " \n", "\x0b", "\x0c", "\u00a0", "\u2003", "\u3000", "\u2028", "\u0085",
	// words, digits, punctuation, quotes and dashes
	"a", "a", "b", "c", "foo", "bar", "x", "A", "Z", "1", "0", "9", "é", "ß", "ǅ", "中", "🙂", "¡", "«", "»", "…", "—",
	"“", "”", "'", "'", "\"", "\"", "--", "---", "----", "..", "...", ".", "!", "?", ",", "{", "}", "{#i}", "{.c}",
	"{k=v}", "{ #i .c k=v }", "{}", "\x00", "\x7f", "\ufeff", "\u200b", "$", "^", "@", "/", "%",
}

// corpusLinePrefixes begin a line of a structured soup: the container
// markers and indentation that decide which block a line belongs to.
var corpusLinePrefixes = []string{
	"", "", "", "", "", "", "> ", "> ", ">", "- ", "- ", "  ", "  ", "    ", "\t", "1. ", "   ", "* ", "+ ", ">> ", "> > ",
	"> - ", "- > ", "  - ", "    - ", "   > ", "[^a]: ", "[a]: ", "| ", "# ", "## ", " ", "     ", "\t\t", " \t", "-\t",
	">\t", "0. ", "1) ", "- [ ] ", "- [x] ", "> # ", "```", "~~~", "    > ", "  1. ",
}

var corpusNewlines = []string{"\n", "\n", "\n", "\n", "\n", "\n", "\n", "\n", "\n\n", "\n\n", "\r\n", "\r"}

// corpusSoup is a run of tokens with no structure at all.
func corpusSoup(r *corpusRand, tokens int) string {
	var out strings.Builder
	for range tokens {
		out.WriteString(r.pick(corpusTokens))
	}
	return out.String()
}

// corpusLineSoup is a few lines, each a container prefix and some tokens.
func corpusLineSoup(r *corpusRand, lines int) string {
	var out strings.Builder
	for range lines {
		out.WriteString(r.pick(corpusLinePrefixes))
		for range r.intn(7) {
			out.WriteString(r.pick(corpusTokens))
		}
		out.WriteString(r.pick(corpusNewlines))
	}
	return out.String()
}

// --- documents built from constructs ----------------------------------------

var corpusWords = []string{"foo", "bar", "baz", "a", "b", "c", "x", "it's", "lorem", "ipsum", "1", "2.", "été", "中文", "Ω", "WORD"}

var corpusLabels = []string{"a", "b", "foo", "Foo", "FOO", "a b", "a  b", "ǅ", "ß", "x1", "1", "*a*", "`a`", "a\\]b", "^n", "\ufb03", "é"}

// corpusInline is a line of inline content.
func corpusInline(r *corpusRand, pieces int) string {
	var out strings.Builder
	for i := range pieces {
		if i > 0 && r.chance(70) {
			out.WriteString(r.pick([]string{" ", " ", " ", "  ", "\t", ""}))
		}
		switch r.intn(34) {
		case 0, 1, 2, 3, 4, 5:
			out.WriteString(r.pick(corpusWords))
		case 6:
			delimiter := r.pick([]string{"*", "_", "**", "__", "***", "___", "~", "~~", "*_", "**_"})
			out.WriteString(delimiter + corpusInline(r, r.between(1, 2)) + corpusReverse(delimiter))
		case 7:
			// delimiter runs that do not pair up
			out.WriteString(r.pick([]string{"*", "**", "_", "__", "~~", "***", "*_*", "**a*", "*a**", "_a__", "a*b*c", "a_b_c", "a~b~c", "* a *", "__a_", "*(*a*)*", "**\"a\"**", "a**\"b\"**c", "*a `*`*", "~~a~b~~", "~~~a~~~"}))
		case 8:
			ticks := strings.Repeat("`", r.between(1, 4))
			body := r.pick([]string{"a", " a ", "  ", "a`b", "a``b", "`", " ` ", "a\\", "a|b", "a\\|b", "*a*", "<a>", "&amp;", "a  b", ""})
			out.WriteString(ticks + body + r.pick([]string{ticks, ticks, ticks, ticks + "`", "", "`"}))
		case 9:
			out.WriteString("[" + corpusInline(r, r.between(0, 2)) + "](" + corpusDestination(r) + corpusTitle(r) + ")")
		case 10:
			out.WriteString("![" + corpusInline(r, r.between(0, 2)) + "](" + corpusDestination(r) + corpusTitle(r) + ")")
		case 11:
			label := r.pick(corpusLabels)
			out.WriteString(r.pick([]string{"[" + label + "]", "[" + label + "][]", "[t][" + label + "]", "![" + label + "]", "![t][" + label + "]", "[" + label + "] [" + label + "]", "[[" + label + "]]", "[" + label + "][" + label + "][" + label + "]", "[" + label + "]()", "[" + label + "]: /x"}))
		case 12:
			out.WriteString(r.pick([]string{"[^a]", "[^b]", "[^ a]", "[^a b]", "[^]", "![^a]", "[^a][^a]", "[^a]: x", "[^A]", "[^1]", "[^a\\]]"}))
		case 13:
			out.WriteString(r.pick([]string{"<http://a.b/c?d=e>", "<a@b.c>", "<mailto:a@b.c>", "<a:b>", "<ab:c d>", "<http://a.b", "<a+b.c-d:e>", "<a@b.c.>", "<a@-b.c>", "<x:>", "<https://é.中/>", "<a.b@c>"}))
		case 14:
			out.WriteString(r.pick([]string{"<b>", "</b>", "<b/>", "<a href=\"x\">", "<a href='x' title=y z>", "<a href=\"x\ny\">", "<a\nb>", "<a b=c\nd='e'>", "<!-- c -->", "<!-->", "<!--->", "<!-- a -- b -->", "<?php ?>", "<? ?", "<!DOCTYPE x>", "<![CDATA[ x ]]>", "<![CDATA[ x ]>", "<b c==d>", "< b>", "<1a>", "<a b=`c`>", "</a b>", "<a/ >", "<a-b_c:d>"}))
		case 15:
			out.WriteString(r.pick([]string{"&amp;", "&lt;", "&copy;", "&#35;", "&#x1F600;", "&#0;", "&#xD800;", "&#99999999;", "&#x;", "&#;", "&;", "&bogus;", "&amp", "&AMP;", "&ClockwiseContourIntegral;", "&ngE;", "&#X41;", "&#x0041;", "&#1114112;"}))
		case 16:
			out.WriteString(r.pick([]string{"\\*", "\\_", "\\\\", "\\`", "\\[", "\\]", "\\(", "\\)", "\\<", "\\>", "\\&", "\\#", "\\|", "\\~", "\\!", "\\a", "\\ ", "\\é", "\\\"", "\\'", "\\-", "\\.", "\\{", "\\}"}))
		case 17:
			out.WriteString(r.pick([]string{"\"a\"", "'a'", "\"a", "a\"", "'", "it's", "'tis", "\"'a'\"", "a--b", "a---b", "----", "-----", "------", "-------", "a...", "....", ". . .", "(\"a\")", "['a']", "\"a\"'b'"}))
		case 18:
			out.WriteString(r.pick([]string{"  \n", "\\\n", " \n", "\n", "   \n", "\t\n", "\\\\\n", "  \r\n", "\\\r\n", "\r"}))
		case 19:
			out.WriteString(r.pick([]string{"\u00a0", "\u2003", "\u3000", "\u2028", "\u0085", "\x0b", "\x0c", "\ufeff", "\u200b", "\u1680", "\u202f"}))
		case 20:
			// punctuation around delimiters decides flanking
			punctuation := r.pick([]string{"¡", "«", "»", "…", "—", "“", "”", "。", "！", "(", ")", ".", ",", "$", "+", "<", "=", ">", "^", "`", "|", "~", "§", "·", "‐"})
			delimiter := r.pick([]string{"*", "_", "**", "__", "~~"})
			out.WriteString(r.pick([]string{
				punctuation + delimiter + "a" + delimiter + punctuation,
				"a" + delimiter + punctuation + "b" + punctuation + delimiter + "c",
				delimiter + punctuation + "a" + punctuation + delimiter,
				"\u00a0" + delimiter + "a" + delimiter + "\u00a0",
				"é" + delimiter + "a" + delimiter + "é",
			}))
		case 21:
			out.WriteString(r.pick([]string{"|", " | ", "\\|", "\\\\|", "`|`", "`\\|`", "[a|b]", "[a](b|c)", "*a|b*", "<a|b>", "\\\\\\|"}))
		case 22:
			out.WriteString(r.pick([]string{"[a](b)[c]", "[a]\n[b]", "[a][\nb]", "[a\nb]", "[a]( b )", "[a](\nb\n)", "[a](b \"c\nd\")", "[a](b\n\"c\")", "[a](<b>)", "[a](<b\nc>)", "[a](b(c)d)", "[a](b(c(d(e(f(g)))))))", "[a](b\\))", "[a](b \"c\"d)", "[a](b 'c' )", "[a](b (c))", "[a]()", "[a](<>)", "[]()", "[](a)", "![](a)", "[a](&amp;)", "[a](\\&)", "[a](b \"&amp;\\\"\")", "[[a](b)](c)", "[![a](b)](c)", "![[a](b)](c)", "[a *b](c)*", "*[a*](b)", "[a `b](c)`", "[a <b c=\"](d)\">"}))
		default:
			out.WriteString(r.pick(corpusWords))
		}
	}
	return out.String()
}

func corpusReverse(s string) string {
	bytes := []byte(s)
	for i, j := 0, len(bytes)-1; i < j; i, j = i+1, j-1 {
		bytes[i], bytes[j] = bytes[j], bytes[i]
	}
	return string(bytes)
}

func corpusDestination(r *corpusRand) string {
	return r.pick([]string{"/u", "/u", "http://a.b/c", "<u v>", "<>", "", "u(v)w", "u\\)v", "#f", "?q=1&r=2", "/é", "u%20v", "<u>v", " /u ", "\n/u\n", "u&amp;v", "\\<u\\>"})
}

func corpusTitle(r *corpusRand) string {
	return r.pick([]string{"", "", "", " \"t\"", " 't'", " (t)", " \"t \\\" u\"", " \"t\nu\"", "\n\"t\"", " \"\"", " 't \"u\"'", " (t (u))", " \"&quot;\"", "\"t\"", " \"t\" x", " \"*t*\""})
}

// corpusLines prefixes every line of text.
func corpusLines(text string, first string, rest string) string {
	lines := strings.SplitAfter(text, "\n")
	var out strings.Builder
	for i, line := range lines {
		if line == "" {
			continue
		}
		if i == 0 {
			out.WriteString(first)
		} else {
			out.WriteString(rest)
		}
		out.WriteString(line)
	}
	return out.String()
}

// corpusBlock is one block-level construct, possibly holding others.
func corpusBlock(r *corpusRand, depth int) string {
	inline := func() string { return corpusInline(r, r.between(1, 4)) }
	switch r.intn(30) {
	case 0, 1, 2:
		// paragraph, with continuation lines that may be lazy or indented
		text := inline() + "\n"
		for range r.intn(3) {
			text += r.pick([]string{"", "", "  ", "    ", "\t", "   "}) + inline() + "\n"
		}
		return text
	case 3:
		level := strings.Repeat("#", r.between(1, 7))
		return r.pick([]string{"", "", " ", "   ", "    "}) + level + r.pick([]string{" ", " ", "  ", "\t", ""}) + inline() +
			r.pick([]string{"", "", " #", " ##", "#", " # ", " \\#", " #\t", " {#id}", " {.cls}", " {#id .c1 .c2 k=v w}", " {}", " { }", "{#a}", " {#a} ", " #{#a}", " {#a} #", " {#<a>}", " {#a\\}", " {a=b=c .d #e #f}", " \\", "  "}) + "\n"
	case 4:
		underline := r.pick([]string{"===", "---", "=", "-", "==", "--", "= =", "- -", "===  ", "   ===", "    ===", "--- -", "***"})
		text := inline() + r.pick([]string{"", "", "  ", " {#id}", " {.c}", "\\"}) + "\n"
		if r.chance(30) {
			text += inline() + "\n"
		}
		return text + underline + "\n"
	case 5:
		return r.pick([]string{"***", "---", "___", "* * *", "- - -", " ***", "   ---", "    ---", "**  * ** * ** * **", "-     -      -      -", "- - - -    ", "_____________________________________", "--", "**", "*-*", "***a", "+++", " _ _ _ ", "*\t*\t*"}) + "\n"
	case 6, 7:
		// bullet list
		marker := r.pick([]string{"-", "-", "*", "+"})
		var text strings.Builder
		for range r.between(1, 3) {
			if r.chance(25) {
				marker = r.pick([]string{"-", "*", "+"})
			}
			lead := r.pick([]string{"", "", "", " ", "  ", "   "})
			gap := r.pick([]string{" ", " ", " ", "  ", "   ", "    ", "     ", "\t"})
			task := r.pick([]string{"", "", "", "", "[ ] ", "[x] ", "[X] ", "[ ]", "[x]a", " [ ] ", "[\t] ", "[  ] "})
			body := inline() + "\n"
			if depth > 0 && r.chance(45) {
				body = corpusBlock(r, depth-1)
			}
			if r.chance(12) {
				body = "\n"
			}
			text.WriteString(corpusLines(task+body, lead+marker+gap, lead+strings.Repeat(" ", r.between(0, 4))))
			if r.chance(25) {
				text.WriteString("\n")
			}
		}
		return text.String()
	case 8, 9:
		// ordered list
		delimiter := r.pick([]string{".", ".", ")"})
		number := r.pickInt([]int{1, 1, 1, 0, 2, 7, 10, 123456789, 1234567890})
		var text strings.Builder
		for range r.between(1, 3) {
			marker := strconv.Itoa(number) + delimiter
			number++
			gap := r.pick([]string{" ", " ", "  ", "    ", "     ", "\t"})
			body := inline() + "\n"
			if depth > 0 && r.chance(40) {
				body = corpusBlock(r, depth-1)
			}
			text.WriteString(corpusLines(body, marker+gap, strings.Repeat(" ", r.between(0, 5))))
			if r.chance(25) {
				text.WriteString("\n")
			}
		}
		return text.String()
	case 10, 11:
		// block quote, with lines that drop the marker (lazy continuation)
		body := corpusBlock(r, max(depth-1, 0))
		if r.chance(40) {
			body += corpusBlock(r, 0)
		}
		lines := strings.SplitAfter(body, "\n")
		var text strings.Builder
		for i, line := range lines {
			if line == "" {
				continue
			}
			marker := r.pick([]string{"> ", "> ", "> ", ">", " > ", "   > ", ">\t", ">  "})
			if i > 0 && r.chance(25) {
				marker = ""
			}
			text.WriteString(marker + line)
		}
		return text.String()
	case 12, 13:
		// fenced code block
		fence := strings.Repeat(r.pick([]string{"`", "`", "~"}), r.between(3, 5))
		lead := r.pick([]string{"", "", "", " ", "  ", "   "})
		info := r.pick([]string{"", "", "rust", " rust ", "rust,no_run", "a b", "a`b", "é", "&amp;", "\\*", "a\\`b", "~~~", "   ", "\ta"})
		closing := r.pick([]string{fence, fence, fence, fence + fence[:1], fence[:len(fence)-1], "", fence + " ", fence + " x", "   " + fence, "    " + fence, fence[:2]})
		var text strings.Builder
		text.WriteString(lead + fence + info + "\n")
		for range r.intn(3) {
			text.WriteString(r.pick([]string{"", "", " ", "  ", "    ", "\t", lead}) + r.pick([]string{"code", "*a*", "", "<b>", "```", "~~~", "> q", "- l", "&amp;", "\\"}) + "\n")
		}
		if closing != "" {
			text.WriteString(closing + "\n")
		}
		return text.String()
	case 14:
		// indented code block
		var text strings.Builder
		for range r.between(1, 3) {
			text.WriteString(r.pick([]string{"    ", "    ", "\t", "     ", "  \t", "    \t", " \t"}) + r.pick([]string{"code", "*a*", "<b>", "- l", "    deep", "&amp;", "\\"}) + "\n")
			if r.chance(25) {
				text.WriteString(r.pick([]string{"\n", "  \n", "    \n", "      \n", "\t\n"}))
			}
		}
		return text.String()
	case 15, 16:
		// HTML blocks of each of the seven kinds
		return r.pick([]string{
			"<script>\nvar a = *b*;\n\n</script> c\n",
			"<pre>\n*a*\n\n*b*\n</pre>\n",
			"<style>\n*{}\n</style>\n*a*\n",
			"<textarea>\n\n*a*\n</textarea>\n",
			"<SCRIPT>\n</SCRIPT>\n*a*\n",
			"<pre\n*a*\n",
			"<!-- c\n\n*a* -->\n*b*\n",
			"<!---->*a*\n",
			"<?php\n\n?>\n*a*\n",
			"<!DOCTYPE html>\n*a*\n",
			"<!x\n\n>\n",
			"<![CDATA[\n*a*\n\n]]>\n*b*\n",
			"<div>\n*a*\n\n*b*\n</div>\n",
			"<div\n  class=\"a\">\n*b*\n",
			"</div>\n*a*\n",
			"<DIV class=x>*a*\n\n*b*\n",
			"<table><tr><td>\n<pre>\n**a**\n\n</td></tr></table>\n",
			"<details>\n<summary>*a*</summary>\n\n*b*\n\n</details>\n",
			"<a href=\"x\">\n*a*\n</a>\n",
			"<a href=\"x\" >  \n*a*\n",
			"<x-y z=\"1\" w>\n*a*\n\n*b*\n",
			"</x-y>\n*a*\n",
			"<a href=\"x\"> b\n*a*\n",
			"<a  /><b2\ndata=\"c\">\n",
			"<i class=\"a\n\">\n*b*\n",
			"  <div>\n  *a*\n\n    <div>\n",
			"<h1>*a*</h1>\n<p", "<hr/>\n*a*", "<img src=x>\n", "<br>\n",
			"<div>\r\n*a*\r\n\r\n*b*\r\n",
			"<del>\n*a*\n</del>\n",
			"<del>*a*</del>\n",
			"<option>\n<optgroup>\n<search>\n",
		})
	case 17, 18:
		// link reference definitions, good and bad
		label := r.pick(corpusLabels)
		return r.pick([]string{
			"[" + label + "]: /u\n",
			"[" + label + "]: /u \"t\"\n",
			"[" + label + "]:\n  /u\n  't'\n",
			"[" + label + "]: <u v> (t)\n",
			"[" + label + "]: /u \"t\nu\"\n",
			"[" + label + "]: /u \"t\n\nu\"\n",
			"[" + label + "]: /u \"t\" x\n",
			"[" + label + "]: /u\n\"t\" x\n",
			"[" + label + "]:\n",
			"[" + label + "]: \n\n/u\n",
			"[" + label + "]: /u\n[" + strings.ToUpper(label) + "]: /v\n",
			"   [" + label + "]: /u\n",
			"    [" + label + "]: /u\n",
			"[" + label + "\n]: /u\n",
			"[\n" + label + "]: /u 't'\n",
			"[" + label + "]: /u\n===\n",
			"[" + label + "]: /u\ntext\n",
			"text\n[" + label + "]: /u\n",
			"[" + label + "]: /u\\\n",
			"[" + label + "]: /u (t\n",
			"[" + label + "]: <u\n",
			"[" + label + "]: /u 't' \n\n",
			"[" + label + "]: /&amp;\\* \"&quot;\\\"\"\n",
			"[" + label + "]: /u\r\n[b]: /v\r\n",
			"[" + label + "]: /u (t (u))\n",
			"[" + label + "]: /u \"t\"\n- x\n",
			"[" + label + "]: /u\n\"t\n> q\n",
		}) + r.pick([]string{"", "\n", "\n[" + label + "]\n", "\n[" + label + "][]\n", "\n[t][" + label + "]\n", "\n![" + label + "]\n"})
	case 19, 20, 21:
		// table
		columns := r.between(1, 4)
		cell := func() string {
			return r.pick([]string{"a", "b", "", " ", "*a*", "`a`", "a\\|b", "`a\\|b`", "`a|b`", "[a](b)", "[a|b]", "a \\\\| b", "<b>", "&amp;", "~~a~~", "a|", "é", "**a", "a**", "\\", "[a][b]", "![a](b \"c|d\")", "*a\\|b*", "\\\\\\|", "``a\\|``"})
		}
		row := func(cells int, style int) string {
			var parts []string
			for range cells {
				parts = append(parts, cell())
			}
			line := strings.Join(parts, r.pick([]string{"|", " | ", " |", "| "}))
			switch style {
			case 0:
				return "| " + line + " |"
			case 1:
				return line
			case 2:
				return "|" + line
			default:
				return line + "|"
			}
		}
		var delimiters []string
		for range columns + r.pickInt([]int{0, 0, 0, 0, 0, 1, -1}) {
			delimiters = append(delimiters, r.pick([]string{"---", "-", ":--", "--:", ":-:", ":", "- -", "", " --- ", "::", "-:-", ":-", "-:"}))
		}
		style := r.intn(4)
		var text strings.Builder
		if r.chance(25) {
			text.WriteString(inline() + "\n")
		}
		text.WriteString(row(columns, style) + "\n")
		delimiterRow := strings.Join(delimiters, "|")
		if style == 0 || r.chance(30) {
			delimiterRow = "|" + delimiterRow + "|"
		}
		text.WriteString(r.pick([]string{"", "", "", " ", "   ", "    "}) + delimiterRow + r.pick([]string{"", "", " ", "\t"}) + "\n")
		for range r.intn(4) {
			text.WriteString(row(max(columns+r.pickInt([]int{0, 0, 0, 1, -1, 2}), 1), r.intn(4)) + "\n")
		}
		if r.chance(20) {
			text.WriteString(r.pick([]string{"text\n", "> q\n", "- l\n", "# h\n", "```\n", "    code\n", "<div>\n", "|\n", "||\n", " \n", "---\n", "[^a]: f\n"}))
		}
		return text.String()
	case 22, 23:
		// footnotes
		label := r.pick([]string{"a", "b", "1", "A", "a b", "é", "a\\]"})
		return r.pick([]string{
			"[^" + label + "]: " + inline() + "\n",
			"[^" + label + "]:\n    " + inline() + "\n",
			"[^" + label + "]: " + inline() + "\n    " + inline() + "\n",
			"[^" + label + "]: " + inline() + "\n\n    " + inline() + "\n",
			"[^" + label + "]: " + inline() + "\n" + inline() + "\n",
			"[^" + label + "]: " + inline() + "\n[^c]: " + inline() + "\n",
			"   [^" + label + "]: " + inline() + "\n",
			"    [^" + label + "]: " + inline() + "\n",
			"[^" + label + "]:\n",
			"[^" + label + "]: - " + inline() + "\n    - " + inline() + "\n",
			"[^" + label + "]: > " + inline() + "\n",
			"> [^" + label + "]: " + inline() + "\n",
			"- [^" + label + "]: " + inline() + "\n",
			"[^" + label + "]: |a|\n    |-|\n",
			"[^" + label + "\n]: " + inline() + "\n",
		}) + r.pick([]string{"", "\n", "\n" + inline() + " [^" + label + "] [^" + strings.ToUpper(label) + "] [^zz]\n", "[^" + label + "]\n"})
	case 24:
		// metadata blocks; only at the left margin (see metadataBlockMayNotReturn)
		return r.pick([]string{
			"---\ntitle: a\n---\n",
			"---\ntitle: a\n...\n",
			"+++\ntitle = \"a\"\n+++\n",
			"---\n\ntitle: a\n---\n",
			"---\n---\n",
			"---\ntitle: a\n",
			"---\n- a\n- b\n---\n*c*\n",
			"+++\na\n---\n",
			"---\na\n+++\n",
			"---\r\na\r\n---\r\n",
			"---\na\n--- \nb\n",
			"---\na\n----\n",
			"----\na\n---\n",
			"---\n    a\n---\n",
			"---\n> a\n---\n",
			"...\na\n...\n",
		})
	case 25:
		return r.pick([]string{"\n", "\n\n", "  \n", "\t\n", "\r\n", "\n \n"})
	default:
		return inline() + "\n"
	}
}

// corpusDocument is a few blocks, sometimes inside a container.
func corpusDocument(r *corpusRand) string {
	var out strings.Builder
	for i := range r.between(1, 4) {
		if i > 0 && r.chance(55) {
			out.WriteString("\n")
		}
		out.WriteString(corpusBlock(r, r.between(0, 2)))
	}
	text := out.String()
	switch r.intn(12) {
	case 0:
		text = strings.ReplaceAll(text, "\n", "\r\n")
	case 1:
		text = strings.ReplaceAll(text, "\n", "\r")
	case 2:
		text = strings.TrimRight(text, "\n")
	case 3:
		text = corpusLines(text, "> ", "> ")
	case 4:
		text = corpusLines(text, "- ", "  ")
	case 5:
		text = corpusLines(text, "1. ", "   ")
	}
	return text
}

// --- mutations -----------------------------------------------------------------

// corpusMutate changes a document a little: the changes that turn one
// construct into another are small ones.
func corpusMutate(r *corpusRand, text string, other string) string {
	runes := []rune(text)
	position := func() int {
		if len(runes) == 0 {
			return 0
		}
		return r.intn(len(runes) + 1)
	}
	for range r.between(1, 3) {
		switch r.intn(13) {
		case 0:
			// delete a character
			if len(runes) > 0 {
				at := r.intn(len(runes))
				runes = append(runes[:at:at], runes[at+1:]...)
			}
		case 1, 2:
			// insert a token
			at := position()
			token := []rune(r.pick(corpusTokens))
			runes = append(runes[:at:at], append(token, runes[at:]...)...)
		case 3:
			// duplicate a span
			if len(runes) > 0 {
				from := r.intn(len(runes))
				to := min(len(runes), from+r.between(1, 6))
				span := append([]rune(nil), runes[from:to]...)
				at := position()
				runes = append(runes[:at:at], append(span, runes[at:]...)...)
			}
		case 4:
			// cut the document short
			runes = runes[:position()]
		case 5:
			// swap two characters
			if len(runes) > 1 {
				a, b := r.intn(len(runes)), r.intn(len(runes))
				runes[a], runes[b] = runes[b], runes[a]
			}
		case 6:
			// turn line feeds into another line ending
			ending := r.pick([]string{"\r\n", "\r", "\n\n", "  \n", "\\\n", " \n"})
			runes = []rune(strings.ReplaceAll(string(runes), "\n", ending))
		case 7:
			// put every line in a container
			prefix := r.pick([]string{"> ", ">", "- ", "  ", "    ", "\t", "1. ", "* ", "> > ", " "})
			rest := prefix
			if r.chance(50) {
				rest = strings.Repeat(" ", len(prefix))
			}
			if r.chance(20) {
				rest = ""
			}
			runes = []rune(corpusLines(string(runes), prefix, rest))
		case 8:
			// a space becomes a tab, or goes
			for i, c := range runes {
				if c == ' ' && r.chance(40) {
					runes[i] = '\t'
				}
			}
		case 9:
			// join with another document
			runes = append(runes, []rune(r.pick([]string{"", "\n", "\n\n", " "})+other)...)
		case 10:
			// change the case of the letters
			if r.chance(50) {
				runes = []rune(strings.ToUpper(string(runes)))
			} else {
				runes = []rune(strings.ToLower(string(runes)))
			}
		case 11:
			// replace a character with a token
			if len(runes) > 0 {
				at := r.intn(len(runes))
				token := []rune(r.pick(corpusTokens))
				runes = append(runes[:at:at], append(token, runes[at+1:]...)...)
			}
		case 12:
			// drop a line
			lines := strings.SplitAfter(string(runes), "\n")
			if len(lines) > 1 {
				at := r.intn(len(lines))
				lines = append(lines[:at:at], lines[at+1:]...)
				runes = []rune(strings.Join(lines, ""))
			}
		}
	}
	return string(runes)
}

// --- fixed documents ---------------------------------------------------------

// corpusSmall is every document of one and two characters, and many of
// three, over the characters the parser treats specially.
func corpusSmall() []string {
	alphabet := []string{"*", "_", "~", "`", "[", "]", "(", ")", "<", ">", "!", "\\", "&", "#", "-", "+", "=", "|", ":", ".", "\n", " ", "\t", "\r", "1", "a", "\"", "'", "^", "{", "}", ";", "/", "é"}
	documents := []string{""}
	documents = append(documents, alphabet...)
	for _, a := range alphabet {
		for _, b := range alphabet {
			documents = append(documents, a+b)
		}
	}
	core := []string{"*", "_", "`", "[", "]", "(", "<", ">", "\n", " ", "a", "\\", "-", "|", "#", "~"}
	for _, a := range core {
		for _, b := range core {
			for _, c := range core {
				documents = append(documents, a+b+c)
			}
		}
	}
	return documents
}

// corpusLong is input that is very long or very deeply nested.
func corpusLong() []string {
	repeat := strings.Repeat
	var nestedList, nestedQuoteList, table, references, footnotes strings.Builder
	for depth := range 40 {
		nestedList.WriteString(repeat("  ", depth) + "- item " + strconv.Itoa(depth) + "\n")
	}
	for depth := range 30 {
		nestedQuoteList.WriteString(repeat("> ", depth) + repeat("  ", depth) + "- > a *b*\n")
	}
	table.WriteString("| a | b | c | d |\n|---|:-:|--:|---|\n")
	for row := range 120 {
		table.WriteString("| " + strconv.Itoa(row) + " | `x\\|y` | *z* | [l](u) |" + repeat(" extra |", row%4) + "\n")
	}
	for i := range 150 {
		n := strconv.Itoa(i)
		references.WriteString("[r" + n + "]: /u" + n + " \"t" + n + "\"\n")
	}
	references.WriteString("\n")
	for i := range 170 {
		n := strconv.Itoa(i)
		references.WriteString("[R" + n + "] [x][r" + n + "] ![r" + n + "][]\n")
	}
	for i := range 80 {
		n := strconv.Itoa(i)
		footnotes.WriteString("[^" + n + "]: note " + n + "\n    more\n\ntext [^" + n + "] [^" + strconv.Itoa(i+1) + "]\n\n")
	}
	return []string{
		repeat("> ", 200) + "deep\n",
		repeat(">", 300) + " deep\n" + repeat(">", 150) + " lazy\nlazier\n",
		nestedList.String(),
		nestedQuoteList.String(),
		repeat("*", 3000) + "a" + repeat("*", 3000),
		repeat("*a ", 800) + repeat("b* ", 800),
		repeat("_a ", 600) + repeat("**b ", 600),
		repeat("[", 2500) + "a" + repeat("]", 2500),
		repeat("[a](", 600) + "u" + repeat(")", 600),
		repeat("![", 900) + repeat("](u)", 900),
		repeat("`a` ``b`` ```c ", 500),
		repeat("`", 700) + " a " + repeat("`", 699),
		repeat("<a href=\"x\">", 400) + repeat("</a>", 400),
		repeat("<!-- ", 800) + repeat("<? ", 800) + repeat("<![CDATA[ ", 500) + repeat("<!X ", 500),
		repeat("&amp;&#35;&bogus;\\*", 600),
		repeat("a ", 12000),
		repeat("word ", 400) + "\n" + repeat("=", 400) + "\n",
		repeat("line\n", 2500),
		repeat("line  \nbreak\\\n", 900),
		repeat("\n", 4000) + "a",
		repeat("- a\n\n", 500),
		repeat("1. a\n", 700),
		repeat("# h\n", 800) + repeat("h\n-\n", 400),
		repeat("```\ncode\n```\n", 400) + "```\n" + repeat("unclosed\n", 400),
		repeat("    code\n\n", 600),
		repeat("<div>\n*a*\n\n", 400),
		table.String(),
		"|" + repeat(" h |", 60) + "\n|" + repeat("-|", 60) + "\n" + repeat("|"+repeat(" c |", 30)+"\n", 60),
		references.String(),
		// the link reference expansion limit (100 000 bytes) runs out part way
		"[big]: /" + repeat("u", 4000) + " \"" + repeat("t", 1000) + "\"\n\n" + repeat("[big] ", 40),
		footnotes.String(),
		repeat("~~a~~ ~b~ ", 900),
		repeat("\"a\" 'b' -- --- ... ", 600),
		repeat("\t", 500) + "a\n" + repeat(" ", 3000) + "b\n",
		repeat("é中🙂¡«» ", 1500),
		repeat("> - 1. ", 60) + "a\n",
		repeat("- ", 400) + "a\n",
		repeat("* [ ] task\n  * [x] done\n", 300),
		repeat("\\", 5001) + "\n" + repeat("\\", 5000) + "\n",
		repeat("a\r\n", 1500) + repeat("b\r", 1500),
		repeat("[a]: /u\n", 300) + repeat("[a]\n", 300),
		repeat("# a {#i .c k=v}\n", 500),
		"---\n" + repeat("key: value\n", 600) + "---\n" + repeat("+++\nx\n+++\n", 200),
		repeat("x", 100000),
	}
}

// corpusEdges are documents written by hand for cases the generators reach
// rarely.
func corpusEdges() []string {
	return []string{
		// setext heading or thematic break or list
		"a\n---\n", "a\n- - -\n", "- a\n---\n", "- a\n  ---\n", "a\n===\nb\n---\n", "a\n\n---\n", "> a\n---\n", "> a\n> ---\n",
		"a\n ---\n", "a\n    ---\n", "a\\\n---\n", "a  \n---\n", "***\n---\n", "- ---\n", "- - - -\n", "* * *\n* * * *\n", "-\n---\n",
		"a\n=\n", "a\n= =\n", "=\n=\n", "a\n\t===\n", "[a]: /u\n===\n", "`a\n===\n`", "*a\n---\n*",
		// lazy continuation
		"> a\nb\n", "> a\n> b\nc\n", "- a\nb\n", "- a\n b\n", "1. a\n   b\n  c\n", "> - a\nb\n", "> a\n---\n", "> a\n    b\n", "> ```\na\n",
		"- a\n\n  b\nc\n", "> > a\n> b\nc\n", "- > a\nb\n", "> a\n- b\n", "> a\n# b\n", "> a\n\nb\n", "- a\n- b\n\n- c\n", "- a\n\n\n- b\n",
		"- \n  a\n", "-\n\n  a\n", "-\n  a\n", "1.\n   a\n", "- a\n   - b\n    - c\n", "10. a\n    b\n", "-    a\n", "-     a\n", "- \ta\n", "-\ta\n",
		">\ta\n", ">\t\ta\n", "-\t\ta\n", " -\ta\n", "\t- a\n", "  \ta\n", " \t a\n", "- a\n\tb\n", "- a\n\n\tb\n", "> \t a\n",
		// emphasis and strikethrough
		"*a**b*c**", "**a*b**c*", "***a** b*", "***a* b**", "*a **b***", "**a *b***", "*a_b*c_", "__a*b__c*", "a*\"b\"*", "a**b**c",
		"a_b_c", "_a_b_", "__a__b", "*a\n*", "*\na*", "* a*", "*a *", "**a\\**", "*a\\*", "a * b * c", "\\*a*", "*[a*](b)", "*`a*`", "*<a b=\"*\">*",
		"~a~", "~~a~~", "~~~a~~~", "~a~~", "~~a~", "a~b~c", "a~~b~~c", "~~a ~b~ c~~", "~ a ~", "~~ a ~~", "*~a*~", "~*a~*", "~~a\n~~", "\\~a~",
		"****a****", "*****a*****", "******a******", "*a*****", "**a*b*c**", "*a **b** c*", "_a __b__ c_", "__a_b_c__", "a***b***c", "*a**b",
		// code spans
		"`a`", "``a``", "` a `", "`  a  `", "` `", "`  `", "`a``b`", "``a`b``", "`a\nb`", "`a\r\nb`", "`a\n\nb`", "`\na\n`", "\\`a`", "`a\\`",
		"`a`b`", "```a```", "```a``", "`a``", "\\``a``", "\\```a``", "`a|b`", "> `a\n> b`", "- `a\n  b`", "> `a\nb`", "`a\n> b`", "*a`*`", "[a`](b)`",
		// fences
		"```\na\n```", "```\na\n", "```\n", "```", "~~~\na\n~~~", "``` rust\na\n```\n", "```rust `x`\na\n```\n", "~~~ a`b\n~~~\n", "````\n```\n````\n",
		"```\na\n````\n", "````\na\n```\n", " ```\n a\n ```\n", "   ```\n   a\n  b\n```\n", "    ```\na\n", "```\n\n\n```\n", "> ```\n> a\nb\n", "- ```\n  a\n- b\n",
		"```\r\na\r\n```\r\n", "```\ra\r```\r", "``` \\*\\`&amp;\n```\n", "~~~~~~\n~~~\n~~~~~~\n", "```a\n```b\n```\n", "``\na\n``\n", "~~\na\n~~\n",
		// html
		"<div>", "<div", "<div>\n", "</div", "<div/>", "<DIV>", "<divv>", "<a>", "<a>\n", "<a> b\n", "<a></a>\n", "<a b>\nc\n", "< a>", "<>", "</>", "<a", "<a\n",
		"<!-", "<!--", "<!-->", "<!--->", "<!---->", "<!-- -- -->", "<?", "<??>", "<?>", "<!A", "<!a>", "<!1>", "<![CDATA[", "<![CDATA[]]>", "<![CDATA[]]",
		"a <b> c", "a <b c=\"d\"> e", "a <b c='d> e", "a <b c=d e> f", "a <b\nc> d", "a <b c=\"\nd\"> e", "> a <b\n> c> d", "> a <b c=\"d\n> e\"> f", "- a <b\n  c> d",
		"a <!-- b\nc --> d", "a <? b\nc ?> d", "a <!B c\nd> e", "a <![CDATA[ b\nc ]]> d", "<a href=\"\\\"\">", "<a href='\\''>", "<a b=c`>", "\\<a>", "<\\a>",
		"<http://a.b>", "<http://a b>", "<a@b.c>", "<a@b>", "<@b.c>", "<a:>", "<aa:>", "<a:b>", "<ab:c>", "<aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:b>", "<a\\@b.c>",
		"<script>a</script>\nb\n", "<pre>\n\na\n", "<style\n", "<textarea>a\n\nb</textarea>c\n", "<pre>a</PRE>\nb\n", "<!-- a -->b\nc\n", "<?a?>b\nc\n",
		"  <!-- a\n  b -->\n", "   <div>\n", "    <div>\n", "> <div>\n> a\n\nb\n", "- <div>\n  a\n\nb\n", "<div>\n> a\n", "a\n<div>\n", "a\n<a>\n", "a\n<pre>\n", "a\n<!-- b -->\n",
		// links and references
		"[a]", "[a][]", "[a][b]", "[a]: /u", "[a]: /u\n\n[a]", "[a]: /u\n\n[A]", "[a]: /u\n\n[a][]", "[a]: /u\n\n[b][a]", "[a]: /u\n\n![a]", "[a]: /u\n[a]: /v\n\n[a]",
		"[a](b)", "[a](b \"c\")", "[a]( b 'c' )", "[a](<b c>)", "[a](<b)c>)", "[a](b)c)", "[a]((b))", "[a](b(c)", "[a](b\\(c)", "[a](b c)", "[a](\"b\")", "[a] (b)",
		"[a]\n(b)", "[a](\nb)", "[a](b\n\"c\")", "[a](b \"c\"\n)", "[a](b \"c\nd\")", "[a](b \"c\n\nd\")", "> [a](b\n> \"c\")", "> [a](b \"c\n> d\")", "- [a](b\n  \"c\")",
		"[a [b](c) d](e)", "[a ![b](c) d](e)", "![a [b](c) d](e)", "[a [b] c]", "[a [b] c](d)", "[a] [b]: /u", "[a\\]", "[a\\]]", "[a\\\\]", "[]", "[ ]", "[\n]",
		"[a]: /u\n\n[a\\]", "[a  b]: /u\n\n[a b] [a   b] [a\nb]", "[a]:/u\n\n[a]", "[a] : /u\n\n[a]", " [a]: /u\n\n[a]", "[a]: /u \"t\" \n\n[a]", "[a]: /u \"t\"x\n\n[a]",
		"[a]: </u> 't'\n\n[a]", "[a]: <>\n\n[a]", "[a]: \n\n[a]", "[a]: /u\n\"t\n\n[a]", "[a]: /u\n\"t\"\nb\n\n[a]", "> [a]: /u\nb\n\n[a]", "> [a]: /u\n> \"t\"\n\n[a]",
		"- [a]: /u\n\n[a]", "[a]: /u\n[b]: /v\n[c]: /w\n\n[a][b][c]", "[a][b][c]\n\n[c]: /w", "[a][b][c]\n\n[b]: /v", "[a]: /u\n\n[a](v) [a] (v) [a][] [a] []",
		"[a\n\nb]: /u", "[" + strings.Repeat("a", 998) + "]: /u\n\n[" + strings.Repeat("a", 998) + "]", "[" + strings.Repeat("a", 1000) + "]: /u\n\n[" + strings.Repeat("a", 1000) + "]",
		"[" + strings.Repeat("é", 999) + "]: /u\n\n[" + strings.Repeat("é", 999) + "]", "[" + strings.Repeat("é", 1000) + "]: /u", "[" + strings.Repeat("\\]", 500) + "]: /u",
		"![a](b)", "![a][b]\n\n[b]: /u", "![a *b* `c` <d> [e](f)](g)", "![a\nb](c)", "![[a]](b)", "!\\[a](b)", "!\n[a](b)", "![a]\n\n[a]: /u \"t\"",
		// tables
		"a|b\n-|-\n", "a|b\n-|-", "|a|b|\n|-|-|\n|c|d|\n", "a|b\n-|-\nc\n", "a|b\n-|-\nc|d|e\n", "a|b\n-|-|-\n", "a|b\n-\n", "a\n-|-\n", "|a|\n|-|\n", "|a\n|-\n", "a|\n-|\n",
		"|\n|-|\n", "||\n|-|\n", "a|b\n:-:|--:\nc|d\n", "a|b\n -|-\n", "a|b\n    -|-\n", "    a|b\n    -|-\n", " a|b\n -|-\n c|d\n", "a|b\n-|-\n\nc|d\n", "a|b\n-|-\n> c\n",
		"a|b\n-|-\n- c\n", "a|b\n-|-\n# c\n", "a|b\n-|-\n```\n", "a|b\n-|-\n<div>\n", "a|b\n-|-\n    c\n", "a|b\n-|-\n---\n", "a\\|b\n-\\|-\n", "a\\|b|c\n-|-\nd\\|e|f\n",
		"`a|b`|c\n-|-\n`d\\|e`|f\n", "a|b\n-|-\n`c|d`\n", "a|b\n-|-\n`c\\|d`|e\n", "a|b\n-|-\n\\`c|d`\n", "a|b\n-|-\n*c|d*\n", "a|b\n-|-\n*c\\|d*|e\n", "a|b\n-|-\n[c|d](e)\n",
		"a|b\n-|-\n[c\\|d](e\\|f \"g\\|h\")|i\n", "a|b\n-|-\n[c\\\\|d]|e\n\n[c\\|d]: /u\n", "a|b\n-|-\n<c|d>\n", "a|b\n-|-\n<a href=\"c|d\">|e\n", "a|b\n-|-\nc\\\\|d\n", "a|b\n-|-\nc\\\\\\|d\n",
		"x\n|a|b|\n|-|-|\n", "x\na|b\n-|-\n", "> a|b\n> -|-\n> c|d\ne|f\n", "- a|b\n  -|-\n  c|d\n", "- a|b\n-|-\n", "a|b\r\n-|-\r\nc|d\r\n", "a|b\r-|-\rc|d\r", "a | b\n- | -\n | \n",
		"|a|b|\n|-|-|\n|*c*|~~d~~|\n|`e`|[f](g)|\n", "a|b\n-|-\n|\n", "a|b\n-|-\n||\n", "a|b\n-|-\n| |\n", "a|b\n-|-\nc|\n", "a|b\n-|-\n|c\n", "a|b\n:|:\n", "a|b\n-:|:-\nc|d",
		"a|b\n-|-\nc|d\n===\n", "a|b\n===\n", "| a |\n| - |\n| b\\\n", "| a |\n| - |\n| b  \n| c |\n", "|a|b|\n|-|-|\n|c|d|e|f|\n|g|\n", "a|*b\n-|-\nc*|d\n", "|[a]|\n|-|\n\n[a]: /u\n",
		// footnotes
		"[^a]\n\n[^a]: b\n", "[^a]: b\n\n[^a]\n", "[^a]", "[^a]: b", "a[^a]\n", "[^a][^b]\n\n[^a]: c\n[^b]: d\n", "[^a]: b\n    c\n\n    d\n\ne\n", "[^a]: b\nc\n", "[^a]:\n    b\n",
		"[^a]: b\n\n\n    c\n", "[^a]: - b\n    - c\n", "[^a]: > b\n", "> [^a]: b\n", "[^a]: [^b]: c\n", "[^a]: b\n[^a]: c\n\n[^a]\n", "[^A]: b\n\n[^a]\n", "[^a\nb]: c\n", "[^ a]: b\n\n[^ a]\n",
		"![^a]\n\n[^a]: b\n", "[^a][]\n\n[^a]: b\n", "[t][^a]\n\n[^a]: b\n", "[^a](b)\n\n[^a]: c\n", "[^a]: /u\n\n[^a]\n", "[^]: a\n\n[^]\n", "[^a]:b\n\n[^a]\n", "[^a] : b\n", "    [^a]: b\n",
		// task lists
		"- [ ] a\n- [x] b\n- [X] c\n", "- [ ]a\n", "- [ ]\n", "- [ ] \n", "-  [ ] a\n", "-     [ ] a\n", "- [  ] a\n", "- [\t] a\n", "- [x]\ta\n", "1. [ ] a\n", "- [ ] a\n\n  b\n", "- [ ] - [x] a\n",
		"- \\[ ] a\n", "- [ ] [ ] a\n", "> - [ ] a\n", "- [ ]\n  a\n", "- [x] *a*\n", "- [ ] # a\n", "- [ ] ```\n", "- [ ] > a\n", "+ [x] a\n", "* [ ] a\n", "[ ] a\n", "- a [ ] b\n",
		// entities and escapes
		"&amp;", "&amp", "&#38;", "&#x26;", "&#X26;", "&#0;", "&#00000038;", "&#12345678;", "&#x110000;", "&#xD800;", "&#xFFFFFF;", "&#x1234567;", "&;", "&#;", "&#x;", "&a;", "&AElig", "&AElig;", "&ffi;",
		"\\&amp;", "&\\amp;", "`&amp;`", "[a](&amp;)", "[a](b \"&amp;\")", "[&amp;]: /u\n\n[&amp;] [&]", "``` &amp;\n```\n", "<a b=\"&amp;\">", "&NotEqualTilde;", "&zwnj;&zwj;", "&nbsp;*a*&nbsp;",
		"\\", "\\\\", "\\\n", "a\\\nb", "a\\\n", "a\\", "\\a", "\\!\\\"\\#\\$\\%\\&\\'\\(\\)\\*\\+\\,\\-\\.\\/\\:\\;\\<\\=\\>\\?\\@\\[\\\\\\]\\^\\_\\`\\{\\|\\}\\~", "\\\t", "\\é", "a\\\\\nb", "a\\\\\\\nb",
		// headings
		"#", "# ", "#\n", "# a", "#a", "####### a", "###### a", "# a #", "# a ##", "# a#", "# a \\#", "# #", "# ##", "#\ta", "# \ta\t#\t", " # a", "    # a", "# a\n# b\n", "## \n", "# *a*\n", "# a\\\n", "# a  \n",
		"# a {#b}\n", "# a {.b}\n", "# a {b}\n", "# a {b=c}\n", "# {#b}\n", "# a {#b} #\n", "# a # {#b}\n", "#{#b}\n", "# a {#b\n", "# a {#b}}\n", "# a {{#b}\n", "# a \\{#b}\n", "# a {#b\\}\n", "# a {#b} \n",
		"a {#b}\n===\n", "a\nb {#c}\n---\n", "a {#b}\nc\n---\n", "a {\n#b}\n===\n", "# a {#b .c .d e=f g}\n", "# a {#b #c}\n", "# a {.}\n", "# a {#}\n", "# a {=}\n", "# a {b=}\n", "# a {=c}\n", "# a { }\n",
		"# a {#é}\n", "# a{#b}\n", "# a {#b}c\n", "# *a* {#b}\n", "# `a {#b}`\n", "# [a](b) {#c}\n", "# a {#b}\r\n", "# a\r{#b}\n", "# a {<b>}\n", "# a <b {#c}>\n",
		// smart punctuation
		"\"a\"", "'a'", "\"a", "a\"", "\"'a'\"", "'\"a\"'", "a's", "'a' 'b'", "\"a\" \"b\"", "\"a 'b' c\"", "--", "---", "----", "-----", "------", "-------", "--------", "---------", "...", "....", ". . .", "a...b",
		"a--b", "a---b", "\\--", "\\...", "`--`", "<a b=\"--\">", "[--](...)", "\"*a*\"", "*\"a\"*", "'[a](b)'", "(\"a\")", "\"a\"'", "''", "\"\"", "'a\"", " ' ", " \" ", "a'b'c", "a\"b\"c", "1'2\"",
		// metadata blocks
		"---\na: b\n---\n", "---\na: b\n...\n", "+++\na\n+++\n", "---\n---\n", "---\n\n---\n", "---\na\n", "a\n---\nb\n---\n", "\n---\na\n---\n", "---\na\n---\nb\n", "---\na\n--- \n", "---\na\n ---\n",
		"--- \na\n---\n", "---\ra\r---\r", "---\r\na\r\n---\r\n", "> ---\n> a\n> ---\n", "- ---\n  a\n  ---\n", "---\n- a\n---\n", "+++\na\n---\n", "---\na\n+++\n...\n", "----\na\n----\n", "---\n\ta\n---\n",
		// characters
		"\x00", "a\x00b", "\ufeff# a\n", "\ufeffa", "a\u00a0b", "\u00a0# a\n", "\u2003- a\n", "*\u00a0a\u00a0*", "\x0b", "\x0c", "a\x0bb", "# a\x0c", "\x0c# a", "- \x0ba", "\x1c", "\u0085a\u0085", "\u2028\u2029",
		"\r", "\r\n", "\r\r", "\n\r", "a\rb", "a\r\nb", "a\r\rb", "a\n\rb", "a  \rb", "a\\\rb", "# a\r", "# a\rb\n", "- a\r- b\r", "> a\r> b\r", "```\ra\r```", "    a\r    b\r", "<div>\r\n\r\na", "a|b\r\n-|-\r\n",
	}
}

// --- the corpora ---------------------------------------------------------------

// corpusRandomDocuments draws count documents: token soups, documents built
// from constructs, and mutations of both and of the specification examples.
func corpusRandomDocuments(r *corpusRand, count int, inputs []suiteInput) []string {
	documents := make([]string, 0, count)
	for len(documents) < count {
		var text string
		switch r.intn(20) {
		case 0, 1, 2, 3:
			text = corpusSoup(r, r.between(2, 28))
		case 4, 5, 6, 7:
			text = corpusLineSoup(r, r.between(1, 6))
		case 8, 9, 10, 11, 12:
			text = corpusDocument(r)
		case 13, 14, 15:
			base := inputs[r.intn(len(inputs))].text
			text = corpusMutate(r, base, inputs[r.intn(len(inputs))].text)
		case 16, 17:
			text = corpusMutate(r, corpusDocument(r), corpusLineSoup(r, 2))
		case 18:
			text = corpusMutate(r, corpusLineSoup(r, r.between(2, 5)), corpusSoup(r, 6))
		default:
			text = corpusInline(r, r.between(2, 9))
		}
		if len(text) > 600 {
			// keep the recorded corpus small; corpusLong covers long input
			continue
		}
		documents = append(documents, text)
	}
	return documents
}

// corpusDocuments is the recorded corpus: fixed documents first, then seeded
// random ones.
func corpusDocuments(inputs []suiteInput) []string {
	var documents []string
	documents = append(documents, corpusSmall()...)
	documents = append(documents, corpusEdges()...)
	for _, input := range inputs {
		documents = append(documents, input.text)
	}
	documents = append(documents, corpusReferenceDocuments()...)
	documents = append(documents, corpusLong()...)
	documents = append(documents, corpusRandomDocuments(&corpusRand{state: 0x6d6a6f6c6e6972}, corpusRandomCount, inputs)...)
	return documents
}

// corpusRandomCount is how many random documents the recorded corpus holds.
// With the fixed documents it comes to more than ten thousand, in a file of
// about two megabytes.
const corpusRandomCount = 5600

// corpusFuzzDocuments is the extra documents MJ_PULLDOWN_CMARK_FUZZ asks a
// live run for.
func corpusFuzzDocuments(t testing.TB, inputs []suiteInput) []string {
	t.Helper()
	setting := os.Getenv("MJ_PULLDOWN_CMARK_FUZZ")
	if setting == "" {
		return nil
	}
	countText, seedText, _ := strings.Cut(setting, ":")
	count, err := strconv.Atoi(countText)
	if err != nil {
		t.Fatalf("MJ_PULLDOWN_CMARK_FUZZ=%q: %v", setting, err)
	}
	seed := uint64(1)
	if seedText != "" {
		if seed, err = strconv.ParseUint(seedText, 10, 64); err != nil {
			t.Fatalf("MJ_PULLDOWN_CMARK_FUZZ=%q: %v", setting, err)
		}
	}
	return corpusRandomDocuments(&corpusRand{state: seed}, count, inputs)
}

// corpusReferenceDocuments are documents about references: defined and
// broken, in each of the three forms, as links and as images.
func corpusReferenceDocuments() []string {
	definitions := "\n\n[a]: /a\n[b]: /b 'title'\n[ǅ]: /dz\n"
	bodies := []string{
		"[a]", "[a][]", "[a][b]", "[b][a]", "[x]", "[x][]", "[x][a]", "[a][x]", "[c]", "[c][]", "[c][d]", "[a][d]", "[d][a]",
		"![a]", "![a][]", "![a][b]", "![x]", "![c]", "![c][]", "![c][d]", "[A]", "[ǆ] [Ǆ] [ǅ]", "[c] [C] [c]", "[c][c][c]", "[c][a][c]",
		"[a] [c] [x] [b]", "[c](d)", "[c]: /late", "[c]\n\n[c]: /late", "See also [`g()`][crate::g].", "See also [`g()`][crate::g][].",
		"[brokenlink1] some other node [brokenlink2]", "[*c*]", "[`c`]", "[c *d*]", "[c\nd]", "[c  d]", "[ c ]", "[c\\]]", "[c\\\\]", "[[c]]",
		"[[c]][d]", "[c [d] e]", "[c][d][e]", "[c] [d]", "[c]\n[d]", "[c][\nd]", "*[c]*", "`[c]`", "<a href=\"[c]\">", "\\[c]", "[c\\]", "![[c]]",
		"[![c]]", "[![c]][d]", "![c *d* `e`][f]", "# [c]\n", "- [c]\n- [d][]\n", "> [c][d]\n", "| [c] | [d][e] |\n|-|-|\n| [f][] | ![g] |\n",
		"[c|d]\n\n|[c\\|d]|\n|-|\n", "[^c]", "[^c][]", "[t][^c]", "[^c]\n\n[^c]: note", "[c](", "[c](d", "[c]()", "[c][", "[c][]]", "[c]]", "[]", "[][]",
		"[][c]", "[c][ ]", "[" + strings.Repeat("c", 1001) + "]", "[c]{.d}", "[c]:", "[c]: ", "[c]:\n", "[xc] [Xc] [cx]", "[c] [c] [xc] [c]",
		"[one][two] [three][] [four] ![five][six] ![seven][] ![eight]", "[c\u00a0d]", "[ß] [SS] [ss]", "[é] [É]", "[c][d]\n\n[D]: /d", "[c][d]\n\n[C]: /c",
	}
	var documents []string
	for _, body := range bodies {
		documents = append(documents, body, body+definitions)
	}
	return documents
}
