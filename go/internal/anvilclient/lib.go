// Port of brokk-anvil-client 0.28.6 src/lib.rs (the part Mjolnir uses).

// Package anvilclient is the part of Brokk's Anvil LLM client (the
// brokk-anvil-client crate, version 0.28.6, library anvil_client) that
// Mjolnir calls: the LlmBackend interface and the model metadata it
// publishes, one-shot structured inference (infer.rs) with its schema
// validation (structured_output.rs), the Codex login file and its token
// refresh (codex_auth.rs), and the provider clients Mjolnir builds for its
// own utility inference (Codex, Grok, Muse, Kimi and the OpenAI-compatible
// client it uses for DeepSeek), with the retry, tool-argument repair,
// Responses API and prompt-cache continuation support they share.
//
// Tool offering is ported only as far as the request and stream shapes:
// Mjolnir never offers tools, so the OpenRouter and Kimi tool-schema
// projections, and structured-output coercion, are not ported. JSON Schema
// validation uses github.com/santhosh-tekuri/jsonschema in place of the
// jsonschema crate, so validation messages fed back to a model differ in
// wording.
//
// Anvil is a separate Rust crate that Mjolnir depends on from crates.io, and
// Go has no equivalent library, so the items Mjolnir uses are ported here,
// one file per Anvil module, from the crate source of the pinned version.
// Items Mjolnir does not use are not ported.
//
// Library replacements: reqwest is net/http (a HeaderMap is an http.Header),
// tokio's CancellationToken a context.Context, and serde_json::Value a JSON
// value in acp-go's jsonvalue representation (`any`).
package anvilclient
