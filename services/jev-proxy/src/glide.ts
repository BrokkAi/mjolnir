// Shared provider boundary: callers supply bundled questions, never client wording.
export async function glide(state: unknown, questions: object, key: string): Promise<Response> {
  try {
    // Cloudflare Workers rejects redirect: "error" with TypeError; use manual and fail closed on 3xx.
    const response = await fetch("https://api.fastino.ai/v1/systemone", {
      method: "POST", redirect: "manual", headers: { "Content-Type": "application/json", "X-API-Key": key },
      body: JSON.stringify({ model: "fastino/GLiDE", state, questions, store: false }),
    });
    if (response.status >= 300 && response.status < 400) {
      await response.body?.cancel();
      return Response.json({ error: "upstream_failure" }, { status: 502 });
    }
    return response;
  } catch {
    return Response.json({ error: "upstream_failure" }, { status: 502 });
  }
}
