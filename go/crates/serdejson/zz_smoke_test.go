package serdejson

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
)

type smokeInner struct {
	A int    `rs:"a"`
	B string `rs:"b" serde:"skip_serializing_if=String::is_empty"`
}

var _ = rs.RegisterStruct[smokeInner]("Inner", `deny_unknown_fields`)

type smokeEnum interface{ isSmokeEnum() }
type smokeEnum_Unit struct{}
type smokeEnum_New struct{ F0 int64 }
type smokeEnum_Tup struct {
	F0 int64
	F1 string
}
type smokeEnum_Str struct {
	X rs.Option[int64] `rs:"x"`
	Y []string         `rs:"y" serde:"default"`
}

func (*smokeEnum_Unit) isSmokeEnum() {}
func (*smokeEnum_New) isSmokeEnum()  {}
func (*smokeEnum_Tup) isSmokeEnum()  {}
func (*smokeEnum_Str) isSmokeEnum()  {}

var _ = rs.RegisterEnum[smokeEnum]("Smoke",
	rs.EnumAttrs(`rename_all = "snake_case"`),
	rs.Variant[smokeEnum_Unit]("Unit", rs.ShapeUnit),
	rs.Variant[smokeEnum_New]("New", rs.ShapeTuple),
	rs.Variant[smokeEnum_Tup]("Tup", rs.ShapeTuple),
	rs.Variant[smokeEnum_Str]("StrV", rs.ShapeStruct),
)

type smokeTagged interface{ isSmokeTagged() }
type smokeTagged_A struct {
	N int64 `rs:"n"`
}
type smokeTagged_B struct{}

func (*smokeTagged_A) isSmokeTagged() {}
func (*smokeTagged_B) isSmokeTagged() {}

var _ = rs.RegisterEnum[smokeTagged]("Tagged",
	rs.EnumAttrs(`tag = "type"`),
	rs.Variant[smokeTagged_A]("A", rs.ShapeStruct),
	rs.Variant[smokeTagged_B]("B", rs.ShapeUnit),
)

func TestSmoke(t *testing.T) {
	s, err := ToString(smokeInner{A: 5})
	t.Logf("%q %v", s, err)
	v, err := FromStr[smokeInner](`{"a": 7, "b": "x"}`)
	t.Logf("%+v %v", v, err)
	_, err = FromStr[smokeInner](`{"a": 7, "c": 1}`)
	t.Logf("%v", err)
	_, err = FromStr[smokeInner](`{"b": "q"}`)
	t.Logf("%v", err)
	for _, e := range []smokeEnum{&smokeEnum_Unit{}, &smokeEnum_New{F0: 3}, &smokeEnum_Tup{F0: 1, F1: "z"}, &smokeEnum_Str{X: rs.Some(int64(2))}} {
		s, err := ToString(e)
		back, err2 := FromStr[smokeEnum](s)
		t.Logf("%s %v -> %#v %v", s, err, back, err2)
	}
	_, err = FromStr[smokeEnum](`"nope"`)
	t.Logf("%v", err)
	for _, e := range []smokeTagged{&smokeTagged_A{N: 1}, &smokeTagged_B{}} {
		s, err := ToString(e)
		back, err2 := FromStr[smokeTagged](s)
		t.Logf("%s %v -> %#v %v", s, err, back, err2)
	}
	_, err = FromStr[smokeTagged](`{"type": "C"}`)
	t.Logf("%v", err)
	val, err := FromStr[Value](`{"a": [1, -2, 3.5, "x", null, true, {"b": {}}], "c": 18446744073709551615}`)
	t.Logf("%#v %v", val, err)
	out, err := ToStringPretty(val)
	t.Logf("\n%s %v", out, err)
	f, err := ToString([]float64{0, 1, 1.5, 1e15, 1e16, 1e-5, 1e-6, -0.0, 123456789.125, 5e-324, 1.7976931348623157e308})
	t.Logf("%s %v", f, err)
}
