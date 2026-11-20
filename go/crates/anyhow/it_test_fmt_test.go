// Port of anyhow 1.0.102 tests/test_fmt.rs

package anyhow

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

func fmtF() error {
	return New(ioErrorNew("PermissionDenied", "oh no!"))
}

func fmtG() error {
	return Context(fmtF(), "f failed")
}

func fmtH() error {
	return Context(fmtG(), "g failed")
}

const expectedAltdisplayF = "oh no!"

const expectedAltdisplayG = "f failed: oh no!"

const expectedAltdisplayH = "g failed: f failed: oh no!"

const expectedDebugF = "oh no!"

const expectedDebugG = "" +
	"f failed\n" +
	"\n" +
	"Caused by:\n" +
	"    oh no!"

const expectedDebugH = "" +
	"g failed\n" +
	"\n" +
	"Caused by:\n" +
	"    0: f failed\n" +
	"    1: oh no!"

const expectedAltdebugF = "" +
	"Custom {\n" +
	"    kind: PermissionDenied,\n" +
	"    error: \"oh no!\",\n" +
	"}"

const expectedAltdebugG = "" +
	"Error {\n" +
	"    context: \"f failed\",\n" +
	"    source: Custom {\n" +
	"        kind: PermissionDenied,\n" +
	"        error: \"oh no!\",\n" +
	"    },\n" +
	"}"

const expectedAltdebugH = "" +
	"Error {\n" +
	"    context: \"g failed\",\n" +
	"    source: Error {\n" +
	"        context: \"f failed\",\n" +
	"        source: Custom {\n" +
	"            kind: PermissionDenied,\n" +
	"            error: \"oh no!\",\n" +
	"        },\n" +
	"    },\n" +
	"}"

func TestItTestFmt_test_display(t *testing.T) {
	assertEq(t, "g failed", fmtH().Error())
	assertEq(t, "g failed", DisplayString(fmtH()))
}

func TestItTestFmt_test_altdisplay(t *testing.T) {
	assertEq(t, expectedAltdisplayF, AlternateString(fmtF()))
	assertEq(t, expectedAltdisplayG, AlternateString(fmtG()))
	assertEq(t, expectedAltdisplayH, AlternateString(fmtH()))
}

func TestItTestFmt_test_debug(t *testing.T) {
	assertEq(t, expectedDebugF, DebugString(fmtF()))
	assertEq(t, expectedDebugG, DebugString(fmtG()))
	assertEq(t, expectedDebugH, DebugString(fmtH()))
}

// The same three forms through the formatter, which is how product code
// reaches them.
func TestItTestFmt_test_forms_through_format(t *testing.T) {
	requireFormatter(t)
	assertEq(t, "g failed", rs.Format("{}", fmtH()))
	assertEq(t, "g failed", rs.ToString(fmtH()))
	assertEq(t, expectedAltdisplayF, rs.Format("{:#}", fmtF()))
	assertEq(t, expectedAltdisplayG, rs.Format("{:#}", fmtG()))
	assertEq(t, expectedAltdisplayH, rs.Format("{:#}", fmtH()))
	assertEq(t, expectedDebugF, rs.Format("{:?}", fmtF()))
	assertEq(t, expectedDebugG, rs.Format("{:?}", fmtG()))
	assertEq(t, expectedDebugH, rs.Format("{:?}", fmtH()))
}

func TestItTestFmt_test_altdebug(t *testing.T) {
	requireFormatter(t)
	assertEq(t, expectedAltdebugF, rs.Format("{:#?}", fmtF()))
	assertEq(t, expectedAltdebugG, rs.Format("{:#?}", fmtG()))
	assertEq(t, expectedAltdebugH, rs.Format("{:#?}", fmtH()))
	assertEq(t, expectedAltdebugH, PrettyDebugString(fmtH()))
}
