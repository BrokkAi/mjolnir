// Port of mj-worker/src/acp/verdict_client.rs: its test-only items.

package worker

// newVerdictClient is the test-only VerdictClient::new: a client for source
// without a Fastino key.
func newVerdictClient(source VerdictSource) (*VerdictClient, error) {
	return verdictClientNewWithKey(source, nil)
}
