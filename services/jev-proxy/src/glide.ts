// Shared provider boundary: callers supply bundled questions, never client wording.
export async function glide(state: unknown, questions: object, key: string): Promise<Response> {
  try {
    const response = await fetch("https://api.fastino.ai/v1/systemone", {
      method: "POST", headers: { "Content-Type": "application/json", "X-API-Key": key },
      body: JSON.stringify({ model: "fastino/GLiDE", state, questions, store: false }),
    });
    return response;
  } catch {
    return Response.json({ error: "upstream_failure" }, { status: 502 });
  }
}
