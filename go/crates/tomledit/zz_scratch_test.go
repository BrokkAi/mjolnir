package tomledit

import (
	"testing"

	"github.com/BrokkAi/mjolnir/go/rs"
	"github.com/BrokkAi/mjolnir/go/rs/collections"
)

func TestScratchSer(t *testing.T) {
	m := collections.BTreeMapNew[string, any]()
	_ = m
	inner := collections.BTreeMapNew[string, int64]()
	inner.Insert("x", 1)
	inner.Insert("y", 2)
	outer := collections.BTreeMapNew[string, collections.BTreeMap[string, int64]]()
	outer.Insert("a", inner)
	s, err := SerToString(&outer)
	t.Logf("%q %v", s, err)
	s, err = SerToStringPretty(&outer)
	t.Logf("%q %v", s, err)
	opt := collections.BTreeMapNew[string, rs.Option[int64]]()
	opt.Insert("n", rs.None[int64]())
	opt.Insert("s", rs.Some[int64](3))
	s, err = SerToString(&opt)
	t.Logf("%q %v", s, err)
	arr := collections.BTreeMapNew[string, []int64]()
	arr.Insert("a", []int64{1, 2, 3})
	s, err = SerToStringPretty(&arr)
	t.Logf("%q %v", s, err)
	d, _ := DatetimeFromStr("1979-05-27T07:32:00Z")
	dm := collections.BTreeMapNew[string, Datetime]()
	dm.Insert("d", d)
	s, err = SerToString(&dm)
	t.Logf("%q %v", s, err)
}
