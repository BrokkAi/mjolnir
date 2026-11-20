package tomledit

import (
	"fmt"
	"testing"
)

func TestSmoke(t *testing.T) {
	inputs := []string{
		"",
		"a = 1\n",
		"# c\n[a]\nb = \"x\" # y\n\n[[c]]\nd = [1, 2,\n 3]\ne = { f = 1.5, g = 1979-05-27T07:32:00Z }\n",
		"a = \n",
		"a = 1\na = 2\n",
		"[a]\n[a]\n",
		"a = \"\\q\"\n",
		"key",
		"[[albums]\n",
		"a = 1_\n",
		"x = 9e99999\n",
	}
	for _, input := range inputs {
		doc, err := DocumentFromStr(input)
		if err != nil {
			fmt.Printf("--- %q\n%s", input, err.Error())
			continue
		}
		out := doc.String()
		fmt.Printf("--- %q -> %q same=%v\n", input, out, out == input)
	}
}
