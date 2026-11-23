// Port of mj-worker-mcp/src/lib.rs.

// Package workermcp holds the stdio MCP servers exposed by the Mjolnir
// worker: mj-agents and mj-review (subagent_mcp.go), the project memory
// server (memory_mcp.go, memory_mcp_history.go), and the line-delimited
// MCP plumbing they share (mcp_stdio.go). It is the Go port of the
// mj-worker-mcp crate (`brokk-mj-worker-mcp`, library `mj_worker_mcp`).
//
// The code was first ported while these modules were part of mj-worker's
// package, so names keep the module prefixes they had there
// (SubagentMCPRunMCPStdio is subagent_mcp::run_mcp_stdio). Like the Rust
// crate, this package depends only on core; the worker package imports it.
package workermcp

import (
	"net"
	"time"
)

// timeoutConn bounds each read and each write on conn separately, as
// set_read_timeout and set_write_timeout do on a Rust socket. Go deadlines
// are absolute, so every operation sets its own. A zero duration leaves that
// direction unbounded. (A copy of the worker package's stand-in, which this
// package cannot import.)
type timeoutConn struct {
	conn  net.Conn
	read  time.Duration
	write time.Duration
}

func (c timeoutConn) Read(buffer []byte) (int, error) {
	if c.read > 0 {
		if err := c.conn.SetReadDeadline(time.Now().Add(c.read)); err != nil {
			return 0, err
		}
	}
	return c.conn.Read(buffer)
}

func (c timeoutConn) Write(buffer []byte) (int, error) {
	if c.write > 0 {
		if err := c.conn.SetWriteDeadline(time.Now().Add(c.write)); err != nil {
			return 0, err
		}
	}
	return c.conn.Write(buffer)
}
