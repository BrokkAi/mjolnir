// Port of mj-core/src/config/dsh_credentials.rs.
//
// DSH's own credential store, read the way DeepSeek Harness reads it.
//
// DSH keeps the API key a person saves in it under `refs` in
// `<DSH home>/.credentials.yaml` (`@deepseek-ai/dsh-credentials-local`), and
// reads `<DSH home>/.env` as its lowest fallback. A DSH profile's home is that
// DSH home, so Mjolnir finds the same key DSH would use.
//
// A DeepSeek account sign-in is a different credential: a token DSH sends in
// `x-dsh-auth-token` to its Messages endpoint. dsh-acp speaks the
// OpenAI-compatible API with a bearer key only, so the sign-in is recognized
// and reported, never sent.
//
// YAML is read with go.yaml.in/yaml/v3 when the bodies are ported.

package core

// dshCredentialsFile is the managed store, beside the `.env` fallback in the
// DSH home.
const dshCredentialsFile = ".credentials.yaml"

const dshDotenvFile = ".env"

// dshAccountRecord is the record DSH's browser sign-in writes
// (`deepseek-account-platform`).
const dshAccountRecord = "deepseek-account-platform/default"

// dshDocumentVersion is the only layout DSH reads, besides the flat one it
// migrates from.
const dshDocumentVersion int64 = 1

// dshStoredCredentials is what DSH's credential store holds for one key.
type dshStoredCredentials struct {
	// APIKey is the key saved under `refs`, when there is one.
	APIKey *string
	// AccountSignedIn is whether DSH is signed in to a DeepSeek account.
	AccountSignedIn bool
}

// dshStored reads name from the store in home. A missing file holds nothing;
// a file DSH itself would refuse to load is an error; the caller decides
// whether another source makes it harmless.
func dshStored(home, name string) (dshStoredCredentials, error) {
	panic("unported: mj-core/src/config/dsh_credentials.rs stored")
}

// dshCredentialsParse is Rust's dsh_credentials::parse (prefixed: `parse`
// collides across modules).
func dshCredentialsParse(text, name string) (dshStoredCredentials, error) {
	panic("unported: mj-core/src/config/dsh_credentials.rs parse")
}

// dshReference reads name from a `refs` section, a decoded YAML value.
func dshReference(section any, name string) (*string, error) {
	panic("unported: mj-core/src/config/dsh_credentials.rs reference")
}

// dshDotenv is name from the `.env` in home, DSH's lowest-ranked layer. DSH
// loads it with Node's util.parseEnv; this reads the same syntax for one
// variable.
func dshDotenv(home, name string) (*string, error) {
	panic("unported: mj-core/src/config/dsh_credentials.rs dotenv")
}

// parseDotenv reads Node's dotenv grammar: `[export ]NAME=value` lines, `#`
// comments, and single, double or backtick quotes that may span lines; double
// quotes expand `\n`. A later assignment replaces an earlier one.
func parseDotenv(text, name string) *string {
	panic("unported: mj-core/src/config/dsh_credentials.rs parse_dotenv")
}
