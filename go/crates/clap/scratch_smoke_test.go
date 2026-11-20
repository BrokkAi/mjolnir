package clap

import (
	"fmt"
	"testing"
)

func TestScratchSmoke(t *testing.T) {
	cmd := CommandNew("prog").Version("1.0").About("does things").
		Arg(ArgNew("verbose").Short('v').Long("verbose").Action(ArgAction_Count).Help("more output")).
		Arg(ArgNew("name").Long("name").ValueName("NAME").Help("a name").DefaultValue("bob")).
		Arg(ArgNew("input").Required(true).Help("input file")).
		Subcommand(CommandNew("sub").About("a subcommand").Arg(ArgNew("flag").Long("flag").Action(ArgAction_SetTrue)))
	for _, argv := range [][]string{
		{"prog", "--help"},
		{"prog", "-h"},
		{"prog", "-V"},
		{"prog", "--nme", "x", "in"},
		{"prog"},
		{"prog", "in", "-vvv", "--name=al"},
		{"prog", "in", "sub", "--flag"},
		{"prog", "in", "subb"},
	} {
		m, err := cmd.Clone().TryGetMatchesFrom(argv)
		if err != nil {
			e := err.(*Error)
			fmt.Printf("== %v kind=%s stderr=%v code=%d\n%s---\n", argv, e.Kind().debugString(), e.UseStderr(), e.ExitCode(), e.String())
			continue
		}
		fmt.Printf("== %v ids=%v\n", argv, m.Ids())
		if v, ok := ArgMatchesGetOne[string](m, "name").Get(); ok {
			fmt.Println("name:", v)
		}
		fmt.Println("count:", m.GetCount("verbose"))
	}
}
