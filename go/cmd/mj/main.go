// Command mj is the Go build of Mjolnir's command-line client, dashboard and daemon.
package main

import (
	"os"

	"github.com/BrokkAi/mjolnir/go/internal/cli"
)

func main() {
	os.Exit(cli.Main(os.Args[1:], os.Stdout, os.Stderr))
}
