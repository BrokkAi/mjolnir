package serde

import "github.com/BrokkAi/mjolnir/go/rs"

// buildCodec fills in the codec of c.typ.
func buildCodec(c *codec) {
	handWritten(c)
	if c.ser != nil && c.de != nil {
		return
	}
	panic(rs.Todo("serde derive engine: " + c.typ.String()))
}
