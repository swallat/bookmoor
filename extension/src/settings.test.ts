import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { endpointUrl } from "./settings.ts";

describe("endpointUrl", () => {
  it("keeps a path prefix and ignores trailing slashes", () => {
    for (const base of ["https://h.example/bookmoor", "https://h.example/bookmoor/"]) {
      assert.equal(endpointUrl({ serverUrl: base, query: [] }, "/v1/sync"), "https://h.example/bookmoor/v1/sync");
    }
    assert.equal(endpointUrl({ serverUrl: "http://10.0.0.2:8787", query: [] }, "/v1/tree"), "http://10.0.0.2:8787/v1/tree");
  });

  it("adds query parameters from the URL and the settings, the latter winning", () => {
    const s = {
      serverUrl: "https://h.example/b?via=proxy",
      query: [
        { name: "p_token", value: "a b&c" },
        { name: " ", value: "ignored" },
      ],
    };
    const u = new URL(endpointUrl(s, "/v1/sync"));
    assert.equal(u.pathname, "/b/v1/sync");
    assert.equal(u.searchParams.get("via"), "proxy");
    assert.equal(u.searchParams.get("p_token"), "a b&c");
    assert.equal([...u.searchParams].length, 2);
    const over = new URL(endpointUrl({ serverUrl: "https://h.example/?via=a#frag", query: [{ name: "via", value: "b" }] }, "/v1/tree"));
    assert.equal(over.searchParams.get("via"), "b");
    assert.equal(over.hash, "");
  });

  it("copes with settings saved before query parameters existed", () => {
    assert.equal(endpointUrl({ serverUrl: "https://h.example", query: undefined as never }, "/v1/tree"), "https://h.example/v1/tree");
  });
});
