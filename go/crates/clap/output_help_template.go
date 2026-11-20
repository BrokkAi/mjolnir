// Port of clap_builder 4.6.0 src/output/help_template.rs

package clap

import (
	"strings"

	"github.com/BrokkAi/mjolnir/go/rs"
)

// outputAutoHelp is AutoHelp: `clap` auto-generated help writer.
type outputAutoHelp struct {
	template *outputHelpTemplate
}

// Public Functions
func outputAutoHelpNew(writer *BuilderStyledStr, cmd *Command, usage *outputUsage, useLong bool) *outputAutoHelp {
	return &outputAutoHelp{
		template: outputHelpTemplateNew(writer, cmd, usage, useLong),
	}
}

func (h *outputAutoHelp) writeHelp() {
	pos := false
	for _, arg := range h.template.cmd.GetPositionals() {
		if shouldShowArg(h.template.useLong, arg) {
			pos = true
			break
		}
	}
	nonPos := false
	for _, arg := range h.template.cmd.getNonPositionals() {
		if shouldShowArg(h.template.useLong, arg) {
			nonPos = true
			break
		}
	}
	subcmds := h.template.cmd.hasVisibleSubcommands()

	template := outputHelpDefaultNoArgsTemplate
	if nonPos || pos || subcmds {
		template = outputHelpDefaultTemplate
	}
	h.template.writeTemplatedHelp(template)
}

const outputHelpDefaultTemplate = "{before-help}{about-with-newline}\n" +
	"{usage-heading} {usage}\n" +
	"\n" +
	"{all-args}{after-help}"

const outputHelpDefaultNoArgsTemplate = "{before-help}{about-with-newline}\n" +
	"{usage-heading} {usage}{after-help}"

const outputHelpShortSize = 4 // See `fn short` for the 4

// outputHelpTemplate is HelpTemplate: help template writer.
//
// Wraps a writer stream providing different methods to generate help for
// `clap` objects.
type outputHelpTemplate struct {
	writer       *BuilderStyledStr
	cmd          *Command
	styles       *BuilderStyles
	usage        *outputUsage
	nextLineHelp bool
	termW        int
	useLong      bool
}

// Public Functions
//
// outputHelpTemplateNew creates a new `HelpTemplate` instance.
func outputHelpTemplateNew(writer *BuilderStyledStr, cmd *Command, usage *outputUsage, useLong bool) *outputHelpTemplate {
	termW := outputHelpTemplateTermW(cmd)
	nextLineHelp := cmd.IsNextLineHelpSet()

	return &outputHelpTemplate{
		writer:       writer,
		cmd:          cmd,
		styles:       cmd.GetStyles(),
		usage:        usage,
		nextLineHelp: nextLineHelp,
		termW:        termW,
		useLong:      useLong,
	}
}

func outputHelpTemplateTermW(cmd *Command) int {
	if w, ok := cmd.getTermWidth().Get(); ok {
		if w == 0 {
			return rs.UsizeMax
		}
		return w
	}
	currentWidth, _ := outputHelpDimensions()
	currentWidth2 := currentWidth.UnwrapOr(100)
	maxWidth := rs.UsizeMax
	if mw, ok := cmd.getMaxTermWidth().Get(); ok && mw != 0 {
		maxWidth = mw
	}
	return min(currentWidth2, maxWidth)
}

// writeTemplatedHelp writes help for an argument to the wrapped stream.
//
// The template system is, on purpose, very simple. Therefore, the tags have
// to be written in the lowercase and without spacing.
func (t *outputHelpTemplate) writeTemplatedHelp(template string) {
	parts := strings.Split(template, "{")
	t.writer.PushStr(parts[0])
	for _, part := range parts[1:] {
		tag, rest, ok := strings.Cut(part, "}")
		if !ok {
			continue
		}
		switch tag {
		case "name":
			t.writeDisplayName()
		case "bin":
			t.writeBinName()
		case "version":
			t.writeVersion()
		case "author":
			t.writeAuthor(false, false)
		case "author-with-newline":
			t.writeAuthor(false, true)
		case "author-section":
			t.writeAuthor(true, true)
		case "about":
			t.writeAbout(false, false)
		case "about-with-newline":
			t.writeAbout(false, true)
		case "about-section":
			t.writeAbout(true, true)
		case "usage-heading":
			t.writer.PushStr(t.styles.GetUsage().Render() + "Usage:" + t.styles.GetUsage().RenderReset())
		case "usage":
			t.writer.pushStyled(t.usage.createUsageNoTitle(nil).UnwrapOrDefault())
		case "all-args":
			t.writeAllArgs()
		case "options":
			// Include even those with a heading as we don't have a good way of
			// handling help_heading in the template.
			t.writeArgs(t.cmd.getNonPositionals(), "options", optionSortKey)
		case "positionals":
			t.writeArgs(t.cmd.GetPositionals(), "positionals", positionalSortKey)
		case "subcommands":
			t.writeSubcommands(t.cmd)
		case "tab":
			t.writer.PushStr(outputTAB)
		case "after-help":
			t.writeAfterHelp()
		case "before-help":
			t.writeBeforeHelp()
		default:
			t.writer.PushStr("{" + tag + "}")
		}
		t.writer.PushStr(rest)
	}
}

// writeDisplayName writes binary name of a Parser Object to the wrapped
// stream.
func (t *outputHelpTemplate) writeDisplayName() {
	displayName := textwrapWrap(
		strings.ReplaceAll(t.cmd.GetDisplayName().UnwrapOrElse(t.cmd.GetName), "{n}", "\n"),
		t.termW,
	)
	t.writer.pushString(displayName)
}

// writeBinName writes binary name of a Parser Object to the wrapped stream.
func (t *outputHelpTemplate) writeBinName() {
	var binName string
	if bn, ok := t.cmd.GetBinName().Get(); ok {
		if strings.Contains(bn, " ") {
			// In case we're dealing with subcommands i.e. git mv is translated to git-mv
			binName = strings.ReplaceAll(bn, " ", "-")
		} else {
			binName = textwrapWrap(strings.ReplaceAll(t.cmd.GetName(), "{n}", "\n"), t.termW)
		}
	} else {
		binName = textwrapWrap(strings.ReplaceAll(t.cmd.GetName(), "{n}", "\n"), t.termW)
	}
	t.writer.pushString(binName)
}

func (t *outputHelpTemplate) writeVersion() {
	version := t.cmd.GetVersion().Or(t.cmd.GetLongVersion())
	if output, ok := version.Get(); ok {
		t.writer.pushString(textwrapWrap(output, t.termW))
	}
}

func (t *outputHelpTemplate) writeAuthor(beforeNewLine bool, afterNewLine bool) {
	if author, ok := t.cmd.GetAuthor().Get(); ok {
		if beforeNewLine {
			t.writer.PushStr("\n")
		}
		t.writer.pushString(textwrapWrap(author, t.termW))
		if afterNewLine {
			t.writer.PushStr("\n")
		}
	}
}

func (t *outputHelpTemplate) writeAbout(beforeNewLine bool, afterNewLine bool) {
	var about rs.Option[BuilderStyledStr]
	if t.useLong {
		about = t.cmd.GetLongAbout().Or(t.cmd.GetAbout())
	} else {
		about = t.cmd.GetAbout()
	}
	if output, ok := about.Get(); ok {
		if beforeNewLine {
			t.writer.PushStr("\n")
		}
		output.replaceNewlineVar()
		output.wrap(t.termW)
		t.writer.pushStyled(output)
		if afterNewLine {
			t.writer.PushStr("\n")
		}
	}
}

func (t *outputHelpTemplate) writeBeforeHelp() {
	var beforeHelp rs.Option[BuilderStyledStr]
	if t.useLong {
		beforeHelp = t.cmd.GetBeforeLongHelp().Or(t.cmd.GetBeforeHelp())
	} else {
		beforeHelp = t.cmd.GetBeforeHelp()
	}
	if output, ok := beforeHelp.Get(); ok {
		output.replaceNewlineVar()
		output.wrap(t.termW)
		t.writer.pushStyled(output)
		t.writer.PushStr("\n\n")
	}
}

func (t *outputHelpTemplate) writeAfterHelp() {
	var afterHelp rs.Option[BuilderStyledStr]
	if t.useLong {
		afterHelp = t.cmd.GetAfterLongHelp().Or(t.cmd.GetAfterHelp())
	} else {
		afterHelp = t.cmd.GetAfterHelp()
	}
	if output, ok := afterHelp.Get(); ok {
		t.writer.PushStr("\n\n")
		output.replaceNewlineVar()
		output.wrap(t.termW)
		t.writer.pushStyled(output)
	}
}

// Arguments

// writeAllArgs writes help for all arguments (options, flags, args,
// subcommands) including titles of a Parser Object to the wrapped stream.
func (t *outputHelpTemplate) writeAllArgs() {
	header := t.styles.GetHeader()

	var pos []*Arg
	for _, a := range t.cmd.GetPositionals() {
		if a.GetHelpHeading().IsNone() && shouldShowArg(t.useLong, a) {
			pos = append(pos, a)
		}
	}
	var nonPos []*Arg
	for _, a := range t.cmd.getNonPositionals() {
		if a.GetHelpHeading().IsNone() && shouldShowArg(t.useLong, a) {
			nonPos = append(nonPos, a)
		}
	}
	subcmds := t.cmd.hasVisibleSubcommands()

	var customHeadings utilFlatSet[string]
	for _, arg := range t.cmd.GetArguments() {
		if heading, ok := arg.GetHelpHeading().Get(); ok {
			customHeadings.insert(heading)
		}
	}

	flatten := t.cmd.IsFlattenHelpSet()

	first := true

	if subcmds && !flatten {
		if !first {
			t.writer.PushStr("\n\n")
		}
		first = false
		helpHeading := t.cmd.GetSubcommandHelpHeading().UnwrapOr("Commands")
		t.writer.PushStr(header.Render() + helpHeading + ":" + header.RenderReset() + "\n")

		t.writeSubcommands(t.cmd)
	}

	if len(pos) != 0 {
		if !first {
			t.writer.PushStr("\n\n")
		}
		first = false
		// Write positional args if any
		helpHeading := "Arguments"
		t.writer.PushStr(header.Render() + helpHeading + ":" + header.RenderReset() + "\n")
		t.writeArgs(pos, "Arguments", positionalSortKey)
	}

	if len(nonPos) != 0 {
		if !first {
			t.writer.PushStr("\n\n")
		}
		first = false
		helpHeading := "Options"
		t.writer.PushStr(header.Render() + helpHeading + ":" + header.RenderReset() + "\n")
		t.writeArgs(nonPos, "Options", optionSortKey)
	}
	if !customHeadings.isEmpty() {
		for _, heading := range customHeadings.intoVec() {
			var args []*Arg
			for _, a := range t.cmd.GetArguments() {
				helpHeading, ok := a.GetHelpHeading().Get()
				if !ok || helpHeading != heading {
					continue
				}
				if shouldShowArg(t.useLong, a) {
					args = append(args, a)
				}
			}

			if len(args) != 0 {
				if !first {
					t.writer.PushStr("\n\n")
				}
				first = false
				t.writer.PushStr(header.Render() + heading + ":" + header.RenderReset() + "\n")
				t.writeArgs(args, heading, optionSortKey)
			}
		}
	}
	if subcmds && flatten {
		cmd := t.cmd.Clone()
		cmd.Build()
		t.writeFlatSubcommands(cmd, &first)
	}
}

// helpOrdEntry is one entry of the BTreeMap the help writer orders
// arguments and subcommands with: the key is (ord, key), and inserting an
// equal key replaces the value, as BTreeMap::insert does.
type helpOrdEntry[V any] struct {
	ord   int
	key   string
	value V
}

func helpOrdInsert[V any](entries []helpOrdEntry[V], ord int, key string, value V) []helpOrdEntry[V] {
	i := 0
	for i < len(entries) {
		e := &entries[i]
		if e.ord == ord && e.key == key {
			e.value = value
			return entries
		}
		if e.ord > ord || (e.ord == ord && e.key > key) {
			break
		}
		i++
	}
	entries = append(entries, helpOrdEntry[V]{})
	copy(entries[i+1:], entries[i:])
	entries[i] = helpOrdEntry[V]{ord: ord, key: key, value: value}
	return entries
}

// writeArgs sorts arguments by length and display order and write their
// help to the wrapped stream.
func (t *outputHelpTemplate) writeArgs(args []*Arg, _category string, sortKey outputHelpArgSortKey) {
	// The shortest an arg can legally be is 2 (i.e. '-x')
	longest := 2
	var ordV []helpOrdEntry[*Arg]

	// Determine the longest
	for _, arg := range args {
		// If it's NextLineHelp we don't care to compute how long it is because it may be
		// NextLineHelp on purpose simply *because* it's so long and would throw off all other
		// args alignment
		if !shouldShowArg(t.useLong, arg) {
			continue
		}
		width := textwrapDisplayWidth(arg.String())
		actualWidth := width
		if arg.GetLong().IsSome() {
			actualWidth = width + outputHelpShortSize
		}
		longest = max(longest, actualWidth)

		ord, key := sortKey(arg)
		ordV = helpOrdInsert(ordV, ord, key, arg)
	}

	nextLineHelp := t.willArgsWrap(args, longest)

	for i, entry := range ordV {
		if i != 0 {
			t.writer.PushStr("\n")
			if nextLineHelp && t.useLong {
				t.writer.PushStr("\n")
			}
		}
		t.writeArg(entry.value, nextLineHelp, longest)
	}
}

// writeArg writes help for an argument to the wrapped stream.
func (t *outputHelpTemplate) writeArg(arg *Arg, nextLineHelp bool, longest int) {
	specVals := t.specVals(arg)

	t.writer.PushStr(outputTAB)
	t.short(arg)
	t.long(arg)
	t.writer.pushStyled(arg.stylizeArgSuffix(t.styles, rs.None[bool]()))
	t.alignToAbout(arg, nextLineHelp, longest)

	var about BuilderStyledStr
	if t.useLong {
		about = arg.GetLongHelp().Or(arg.GetHelp()).UnwrapOrDefault()
	} else {
		about = arg.GetHelp().Or(arg.GetLongHelp()).UnwrapOrDefault()
	}

	t.help(arg, about, specVals, nextLineHelp, longest)
}

// short writes argument's short command to the wrapped stream.
func (t *outputHelpTemplate) short(arg *Arg) {
	literal := t.styles.GetLiteral()

	if s, ok := arg.GetShort().Get(); ok {
		t.writer.PushStr(literal.Render() + "-" + string(s) + literal.RenderReset())
	} else if arg.GetLong().IsSome() {
		t.writer.PushStr("    ")
	}
}

// long writes argument's long command to the wrapped stream.
func (t *outputHelpTemplate) long(arg *Arg) {
	literal := t.styles.GetLiteral()
	if long, ok := arg.GetLong().Get(); ok {
		if arg.GetShort().IsSome() {
			t.writer.PushStr(", ")
		}
		t.writer.PushStr(literal.Render() + "--" + long + literal.RenderReset())
	}
}

// alignToAbout writes spaces to the wrapped stream to align the argument's
// help.
func (t *outputHelpTemplate) alignToAbout(arg *Arg, nextLineHelp bool, longest int) {
	var padding int
	if t.useLong || nextLineHelp {
		// long help prints messages on the next line so it doesn't need to align text
		padding = 0
	} else if !arg.IsPositional() {
		selfLen := textwrapDisplayWidth(arg.String()) + outputHelpShortSize
		// Since we're writing spaces from the tab point we first need to know if we
		// had a long and short, or just short
		var pad int
		if arg.GetLong().IsSome() {
			// Only account 4 after the val
			pad = outputTABWidth
		} else {
			// Only account for ', --' + 4 after the val
			pad = outputTABWidth + 4
		}
		padding = usizeSub(longest+pad, selfLen)
	} else {
		selfLen := textwrapDisplayWidth(arg.String())
		pad := outputTABWidth
		padding = usizeSub(longest+pad, selfLen)
	}

	t.writePadding(padding)
}

// help writes argument's help to the wrapped stream.
func (t *outputHelpTemplate) help(arg *Arg, about BuilderStyledStr, specVals string, nextLineHelp bool, longest int) {
	literal := t.styles.GetLiteral()

	// Is help on next line, if so then indent
	if nextLineHelp {
		t.writer.PushStr("\n")
		t.writer.PushStr(outputTAB)
		t.writer.PushStr(outputHelpNextLineIndent)
	}

	var spaces int
	if nextLineHelp {
		spaces = len(outputTAB) + len(outputHelpNextLineIndent)
	} else {
		spaces = longest + outputTABWidth*2
	}
	trailingIndent := spaces // Don't indent any further than the first line is indented
	trailingIndent2 := t.getSpaces(trailingIndent)

	help := about
	helpIsEmpty := help.isEmpty()
	help.replaceNewlineVar()
	nextLineSpecs := t.useLong && arg != nil
	if specVals != "" && !nextLineSpecs {
		if !helpIsEmpty {
			sep := " "
			help.PushStr(sep)
		}
		help.PushStr(specVals)
		helpIsEmpty = help.isEmpty()
	}

	availChars := t.termW - spaces
	if availChars < 0 {
		availChars = 0
	}
	help.wrap(availChars)
	help.indent("", trailingIndent2)
	t.writer.pushStyled(help)

	hasPossibleValues := false
	if arg != nil {
		if !arg.IsHidePossibleValuesSet() && t.useLongPv(arg) {
			const dashSpace = len("- ")
			possibleVals := arg.GetPossibleValues()
			if len(possibleVals) != 0 {
				hasPossibleValues = true
				longest := -1
				for i := range possibleVals {
					f := &possibleVals[i]
					if f.IsHideSet() {
						continue
					}
					longest = max(longest, textwrapDisplayWidth(f.GetName()))
				}
				if longest < 0 {
					rs.PanicAt(1, "Only called with possible value")
				}

				spaces := spaces + outputTABWidth - dashSpace
				trailingIndent := spaces + dashSpace
				trailingIndent2 := t.getSpaces(trailingIndent)

				if !helpIsEmpty {
					t.writer.PushStr("\n\n" + strings.Repeat(" ", spaces))
				}
				t.writer.PushStr("Possible values:")
				for i := range possibleVals {
					pv := &possibleVals[i]
					if pv.IsHideSet() {
						continue
					}
					name := pv.GetName()

					descr := BuilderStyledStrNew()
					descr.PushStr(literal.Render() + name + literal.RenderReset())
					if help, ok := pv.GetHelp().Get(); ok {
						// To align help messages
						padding := usizeSub(longest, textwrapDisplayWidth(name))
						descr.PushStr(": " + strings.Repeat(" ", padding))
						descr.pushStyled(help)
					}

					var availChars int
					if t.termW > len(trailingIndent2) {
						availChars = t.termW - len(trailingIndent2)
					} else {
						availChars = rs.UsizeMax
					}
					descr.replaceNewlineVar()
					descr.wrap(availChars)
					descr.indent("", trailingIndent2)
					t.writer.PushStr("\n" + strings.Repeat(" ", spaces) + "- ")
					t.writer.pushStyled(descr)
				}
			}
		}
	}

	if specVals != "" && nextLineSpecs {
		help := BuilderStyledStrNew()
		if !helpIsEmpty || hasPossibleValues {
			sep := "\n\n"
			help.PushStr(sep)
		}
		help.PushStr(specVals)
		help.wrap(availChars)
		help.indent("", trailingIndent2)
		t.writer.pushStyled(help)
	}
}

// willArgsWrap reports whether the help for any of the arguments will
// wrap.
func (t *outputHelpTemplate) willArgsWrap(args []*Arg, longest int) bool {
	for _, arg := range args {
		if !shouldShowArg(t.useLong, arg) {
			continue
		}
		specVals := t.specVals(arg)
		if t.argNextLineHelp(arg, specVals, longest) {
			return true
		}
	}
	return false
}

func (t *outputHelpTemplate) argNextLineHelp(arg *Arg, specVals string, longest int) bool {
	if t.nextLineHelp || arg.IsNextLineHelpSet() || t.useLong {
		// setting_next_line
		return true
	}
	// force_next_line
	h := arg.GetHelp().Or(arg.GetLongHelp()).UnwrapOrDefault()
	hW := h.displayWidth() + textwrapDisplayWidth(specVals)
	taken := longest + outputTABWidth*2
	return t.termW >= taken &&
		(usizeToF32(taken)/usizeToF32(t.termW)) > 0.40 &&
		hW > (t.termW-taken)
}

func (t *outputHelpTemplate) specVals(a *Arg) string {
	ctx := t.styles.GetContext()
	ctxVal := t.styles.GetContextValue()
	valSep := ctx.Render() + ", " + ctx.RenderReset() // context values styled separator

	var specVals []string
	if env, ok := a.env.Get(); ok {
		if !a.IsHideEnvSet() {
			envVal := ""
			if !a.IsHideEnvValuesSet() {
				value := ""
				if v, ok := env.value.Get(); ok {
					value = toStringLossy(v)
				}
				envVal = "=" + value
			}
			envInfo := ctx.Render() + "[env: " + ctx.RenderReset() + ctxVal.Render() +
				toStringLossy(env.name) + envVal + ctxVal.RenderReset() + ctx.Render() + "]" + ctx.RenderReset()
			specVals = append(specVals, envInfo)
		}
	}
	if a.isTakesValueSet() && !a.IsHideDefaultValueSet() && len(a.defaultVals) != 0 {
		dvs := make([]string, len(a.defaultVals))
		for i, dv := range a.defaultVals {
			dvs[i] = utilEscape(toStringLossy(dv))
		}

		specVals = append(specVals, ctx.Render()+"[default: "+ctx.RenderReset()+ctxVal.Render()+
			strings.Join(dvs, " ")+ctxVal.RenderReset()+ctx.Render()+"]"+ctx.RenderReset())
	}

	var als []string
	for _, alias := range a.shortAliases {
		if alias.visible {
			als = append(als, ctxVal.Render()+"-"+string(alias.name)+ctxVal.RenderReset())
		}
	}
	for _, alias := range a.aliases {
		if alias.visible {
			als = append(als, ctxVal.Render()+"--"+alias.name+ctxVal.RenderReset())
		}
	}

	if len(als) != 0 {
		als := strings.Join(als, valSep)
		specVals = append(specVals, ctx.Render()+"[aliases: "+ctx.RenderReset()+als+ctx.Render()+"]"+ctx.RenderReset())
	}

	if !a.IsHidePossibleValuesSet() && !t.useLongPv(a) {
		possibleVals := a.GetPossibleValues()
		if len(possibleVals) != 0 {
			var pvs []string
			for i := range possibleVals {
				if pv, ok := possibleVals[i].getVisibleQuotedName().Get(); ok {
					pvs = append(pvs, ctxVal.Render()+pv+ctxVal.RenderReset())
				}
			}

			specVals = append(specVals, ctx.Render()+"[possible values: "+ctx.RenderReset()+
				strings.Join(pvs, valSep)+ctx.Render()+"]"+ctx.RenderReset())
		}
	}
	connector := " "
	if t.useLong {
		connector = "\n"
	}
	return strings.Join(specVals, connector)
}

func (t *outputHelpTemplate) getSpaces(n int) string {
	return strings.Repeat(" ", n)
}

func (t *outputHelpTemplate) writePadding(amount int) {
	t.writer.PushStr(strings.Repeat(" ", amount))
}

func (t *outputHelpTemplate) useLongPv(arg *Arg) bool {
	if !t.useLong {
		return false
	}
	possibleValues := arg.GetPossibleValues()
	for i := range possibleValues {
		if possibleValues[i].shouldShowHelp() {
			return true
		}
	}
	return false
}

// Subcommands

// writeFlatSubcommands writes help for subcommands of a Parser Object to
// the wrapped stream.
func (t *outputHelpTemplate) writeFlatSubcommands(cmd *Command, first *bool) {
	header := t.styles.GetHeader()

	var ordV []helpOrdEntry[*Command]
	for _, subcommand := range cmd.GetSubcommands() {
		if !shouldShowSubcommand(subcommand) {
			continue
		}
		ordV = helpOrdInsert(ordV, subcommand.GetDisplayOrder(), subcommand.GetName(), subcommand)
	}
	for _, entry := range ordV {
		subcommand := entry.value
		if !*first {
			t.writer.PushStr("\n\n")
		}
		*first = false

		heading := subcommand.getUsageNameFallback()
		about := subcommand.GetAbout().Or(subcommand.GetLongAbout()).UnwrapOrDefault()

		t.writer.PushStr(header.Render() + heading + ":" + header.RenderReset())
		if !about.isEmpty() {
			t.writer.PushStr("\n" + about.String())
		}

		var args []*Arg
		for _, arg := range subcommand.GetArguments() {
			if shouldShowArg(t.useLong, arg) && !arg.IsGlobalSet() {
				args = append(args, arg)
			}
		}
		if len(args) != 0 {
			t.writer.PushStr("\n")
		}

		subHelp := &outputHelpTemplate{
			writer:       t.writer,
			cmd:          subcommand,
			styles:       t.styles,
			usage:        t.usage,
			nextLineHelp: t.nextLineHelp,
			termW:        t.termW,
			useLong:      t.useLong,
		}
		subHelp.writeArgs(args, heading, optionSortKey)
		if subcommand.IsFlattenHelpSet() {
			subHelp.writeFlatSubcommands(subcommand, first)
		}
	}
}

// writeSubcommands writes help for subcommands of a Parser Object to the
// wrapped stream.
func (t *outputHelpTemplate) writeSubcommands(cmd *Command) {
	literal := t.styles.GetLiteral()

	// The shortest an arg can legally be is 2 (i.e. '-x')
	longest := 2
	type scEntry struct {
		styled BuilderStyledStr
		cmd    *Command
	}
	var ordV []helpOrdEntry[scEntry]
	for _, subcommand := range cmd.GetSubcommands() {
		if !shouldShowSubcommand(subcommand) {
			continue
		}
		styled := BuilderStyledStrNew()
		name := subcommand.GetName()
		styled.PushStr(literal.Render() + name + literal.RenderReset())
		if short, ok := subcommand.GetShortFlag().Get(); ok {
			styled.PushStr(", " + literal.Render() + "-" + string(short) + literal.RenderReset())
		}
		if long, ok := subcommand.GetLongFlag().Get(); ok {
			styled.PushStr(", " + literal.Render() + "--" + long + literal.RenderReset())
		}
		longest = max(longest, styled.displayWidth())
		ordV = helpOrdInsert(ordV, subcommand.GetDisplayOrder(), string(styled), scEntry{styled: styled, cmd: subcommand})
	}

	nextLineHelp := t.willSubcommandsWrap(cmd.GetSubcommands(), longest)

	for i, entry := range ordV {
		if 0 < i {
			t.writer.PushStr("\n")
		}
		t.writeSubcommand(entry.value.styled, entry.value.cmd, nextLineHelp, longest)
	}
}

// willSubcommandsWrap reports whether the help for any of the subcommands
// will wrap.
func (t *outputHelpTemplate) willSubcommandsWrap(subcommands []*Command, longest int) bool {
	for _, subcommand := range subcommands {
		if !shouldShowSubcommand(subcommand) {
			continue
		}
		specVals := t.scSpecVals(subcommand)
		if t.subcommandNextLineHelp(subcommand, specVals, longest) {
			return true
		}
	}
	return false
}

func (t *outputHelpTemplate) writeSubcommand(scStr BuilderStyledStr, cmd *Command, nextLineHelp bool, longest int) {
	specVals := t.scSpecVals(cmd)

	about := cmd.GetAbout().Or(cmd.GetLongAbout()).UnwrapOrDefault()

	t.subcmd(scStr, nextLineHelp, longest)
	t.help(nil, about, specVals, nextLineHelp, longest)
}

func (t *outputHelpTemplate) scSpecVals(a *Command) string {
	ctx := t.styles.GetContext()
	ctxVal := t.styles.GetContextValue()
	valSep := ctx.Render() + ", " + ctx.RenderReset() // context values styled separator
	var specVals []string

	var shortAls []string
	for _, s := range a.GetVisibleShortFlagAliases() {
		shortAls = append(shortAls, ctxVal.Render()+"-"+string(s)+ctxVal.RenderReset())
	}
	for _, s := range a.GetVisibleLongFlagAliases() {
		shortAls = append(shortAls, ctxVal.Render()+"--"+s+ctxVal.RenderReset())
	}
	for _, s := range a.GetVisibleAliases() {
		shortAls = append(shortAls, ctxVal.Render()+s+ctxVal.RenderReset())
	}
	allAls := strings.Join(shortAls, valSep)
	if allAls != "" {
		specVals = append(specVals, ctx.Render()+"[aliases: "+ctx.RenderReset()+allAls+ctx.Render()+"]"+ctx.RenderReset())
	}

	return strings.Join(specVals, " ")
}

func (t *outputHelpTemplate) subcommandNextLineHelp(cmd *Command, specVals string, longest int) bool {
	// Ignore `self.use_long` since subcommands are only shown as short help
	if t.nextLineHelp {
		// setting_next_line
		return true
	}
	// force_next_line
	h := cmd.GetAbout().Or(cmd.GetLongAbout()).UnwrapOrDefault()
	hW := h.displayWidth() + textwrapDisplayWidth(specVals)
	taken := longest + outputTABWidth*2
	return t.termW >= taken &&
		(usizeToF32(taken)/usizeToF32(t.termW)) > 0.40 &&
		hW > (t.termW-taken)
}

// subcmd writes subcommand to the wrapped stream.
func (t *outputHelpTemplate) subcmd(scStr BuilderStyledStr, nextLineHelp bool, longest int) {
	t.writer.PushStr(outputTAB)
	t.writer.pushStyled(scStr)
	if !nextLineHelp {
		width := scStr.displayWidth()

		padding := usizeSub(longest+outputTABWidth, width)
		t.writePadding(padding)
	}
}

const outputHelpNextLineIndent = "        "

// outputHelpArgSortKey is ArgSortKey.
type outputHelpArgSortKey func(arg *Arg) (int, string)

func positionalSortKey(arg *Arg) (int, string) {
	return arg.GetIndex().UnwrapOr(0), ""
}

func optionSortKey(arg *Arg) (int, string) {
	// Formatting key like this to ensure that:
	// 1. Argument has long flags are printed just after short flags.
	// 2. For two args both have short flags like `-c` and `-C`, the
	//    `-C` arg is printed just after the `-c` arg
	// 3. For args without short or long flag, print them at last(sorted
	//    by arg name).
	// Example order: -a, -b, -B, -s, --select-file, --select-folder, -x

	var key string
	if x, ok := arg.GetShort().Get(); ok {
		lower := x
		if 'A' <= x && x <= 'Z' {
			lower = x + ('a' - 'A')
		}
		s := string(lower)
		if 'a' <= x && x <= 'z' {
			s += "0"
		} else {
			s += "1"
		}
		key = s
	} else if x, ok := arg.GetLong().Get(); ok {
		key = x
	} else {
		key = "{" + arg.GetId().AsStr()
	}
	return arg.GetDisplayOrder(), key
}

// outputHelpDimensions is `dimensions` without the crate's `wrap_help`
// feature: the terminal size is never asked for.
func outputHelpDimensions() (rs.Option[int], rs.Option[int]) {
	if featureWrapHelp {
		panic(rs.Todo("clap_builder::output::help_template::dimensions: the `wrap_help` feature (terminal_size and COLUMNS/LINES) is off in Mjolnir's build"))
	}
	return rs.None[int](), rs.None[int]()
}

func shouldShowArg(useLong bool, arg *Arg) bool {
	if arg.IsHideSet() {
		return false
	}
	return (!arg.IsHideLongHelpSet() && useLong) ||
		(!arg.IsHideShortHelpSet() && !useLong) ||
		arg.IsNextLineHelpSet()
}

func shouldShowSubcommand(subcommand *Command) bool {
	return !subcommand.IsHideSet()
}

// usizeSub is `a - b` on usize: it panics on overflow as Rust's debug
// builds do.
func usizeSub(a, b int) int {
	if b > a {
		rs.PanicAt(1, "attempt to subtract with overflow")
	}
	return a - b
}

// usizeToF32 is `n as f32`.
func usizeToF32(n int) float32 {
	return float32(n)
}
