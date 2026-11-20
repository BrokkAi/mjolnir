// Port of mj-worker/src/memory_mcp.rs.

// MCP server for project history search and code provenance.

package worker

import (
	"errors"
	"io"
	"os"

	"github.com/BrokkAi/acp-go/jsonvalue"
)

// MemoryMCPRunMCPStdio is memory_mcp::run_mcp_stdio: serve the history tools
// over MCP's JSON-lines stdio transport. historySocket is the worker's
// history socket, nil when there is none.
func MemoryMCPRunMCPStdio(historySocket *string) error {
	return memoryMCPServe(historySocket, os.Stdin, os.Stdout)
}

func memoryMCPServe(historySocket *string, reader io.Reader, writer io.Writer) error {
	client := newHistoryClient(historySocket)
	tools := memoryMCPDefinitions(client.enabled())
	return Serve(newHistoryShutdownReader(reader, client), writer, MCPServer{
		Name:             "mj-memory",
		Tools:            tools,
		ProgressInterval: ProgressInterval,
		Call: func(params any, present bool, _ *Progress) (any, bool, error) {
			if !present {
				return nil, false, errors.New("tools/call is missing params")
			}
			return client.call(params)
		},
	})
}

// ToolNames is the tools the server lists when it has a history socket,
// which is what a harness that asks before an MCP tool is allowed.
func ToolNames() []string {
	names := []string{}
	for _, tool := range historyToolDefinitions() {
		object, ok := tool.(jsonvalue.Object)
		if !ok {
			continue
		}
		if name, ok := object.Get("name"); ok {
			if text, ok := name.(string); ok {
				names = append(names, text)
			}
		}
	}
	return names
}

func memoryMCPDefinitions(history bool) []any {
	if history {
		return historyToolDefinitions()
	}
	return []any{}
}
