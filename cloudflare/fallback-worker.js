// Cloudflare Worker on the tunnel's hostname (route `relay.example.com/*`): sends each request
// through the tunnel as usual, and when the tunnel can't deliver it, through frp instead.
//
// Env: FORWARD_SECRET, the relay's forwarding secret (`forward_secret` in GET /admin/frp), and
// HOP_URL, the workers.dev address of fallback-hop.js, through which the retry goes out to frp. Every answer from the relay
// carries `x-api-relay`; one without it (Cloudflare's 530 for a dropped tunnel, a 502, no answer
// at all) means the request never reached the relay, so retrying it can't run it twice.

export default {
  async fetch(req, env) {
    // A body can only be read once; keep it for the retry
    const body = req.method === "GET" || req.method === "HEAD" ? null : await req.arrayBuffer();
    const init = (headers) => ({ method: req.method, headers, body, redirect: "manual" });

    // For testing the fallback without taking the tunnel down: skip the tunnel when the request
    // carries the forwarding secret in x-relay-force-fallback
    const forced = env.FORWARD_SECRET && req.headers.get("x-relay-force-fallback") === env.FORWARD_SECRET;
    let primary = null;
    if (!forced) {
      try {
        primary = await fetch(req.url, init(req.headers));
        if (primary.headers.has("x-api-relay")) return primary;
      } catch (e) {
        primary = null;
      }
    }

    const url = new URL(req.url);
    const fallback = new URL(url.pathname + url.search, env.HOP_URL);
    const headers = new Headers(req.headers);
    headers.delete("x-relay-force-fallback");
    headers.delete("host");
    // The relay sees this Worker's address on the frp side; name the caller and prove it
    headers.set("x-relay-client-ip", req.headers.get("cf-connecting-ip") || "");
    headers.set("x-relay-forward-secret", env.FORWARD_SECRET);
    try {
      const resp = await fetch(fallback, init(headers));
      const out = new Response(resp.body, resp);
      out.headers.set("x-relay-path", "frp");
      return out;
    } catch (e) {
      return primary || new Response(JSON.stringify({ type: "error", error: { type: "api_error", message: "relay unreachable" } }),
        { status: 502, headers: { "content-type": "application/json" } });
    }
  },
};
