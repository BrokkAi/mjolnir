// Port of clap_builder 4.6.0 src/output/help.rs and src/output/mod.rs

package clap

const outputTAB = "  "
const outputTABWidth = len(outputTAB)

// outputWriteHelp writes the parser's help to the wrapped stream.
func outputWriteHelp(writer *BuilderStyledStr, cmd *Command, usage *outputUsage, useLong bool) {
	if h, ok := cmd.getOverrideHelp().Get(); ok {
		writer.pushStyled(h)
	} else {
		if tmpl, ok := cmd.getHelpTemplate().Get(); ok {
			outputHelpTemplateNew(writer, cmd, usage, useLong).
				writeTemplatedHelp(tmpl.asStyledStr())
		} else {
			outputAutoHelpNew(writer, cmd, usage, useLong).writeHelp()
		}
	}

	// Remove any lines from unused sections
	writer.trimStartLines()
	// Remove any whitespace caused by book keeping
	writer.trimEnd()
	// Ensure there is still a trailing newline
	writer.PushStr("\n")
}
