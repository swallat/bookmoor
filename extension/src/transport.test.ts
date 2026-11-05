import assert from "node:assert/strict";
import { test } from "node:test";
import { HttpError, LoginRequired } from "./flow.ts";
import { httpTransport, isLoginPage } from "./transport.ts";
import type { Settings } from "./settings.ts";

const settings = (): Settings => ({ serverUrl: "https://b.example", token: "t", headers: [], query: [] });

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

test("cookies are sent and redirects are not followed", async () => {
  let init: RequestInit | undefined;
  const t = httpTransport(settings(), (async (_u: unknown, i?: RequestInit) => {
    init = i;
    return json({ rev: 1, nodes: [] });
  }) as typeof fetch);
  await t.tree();
  assert.equal(init?.credentials, "include");
  assert.equal(init?.redirect, "manual");
});

test("a redirect or an HTML page means login required", async () => {
  const redirect = { type: "opaqueredirect" as const, status: 0, headers: new Headers() };
  assert.ok(isLoginPage(redirect));
  assert.ok(isLoginPage({ type: "basic" as const, status: 302, headers: new Headers() }));
  const html = new Response("<html>", { status: 200, headers: { "content-type": "text/html; charset=utf-8" } });
  assert.ok(isLoginPage(html));
  assert.ok(!isLoginPage(json({})));
  const t = httpTransport(settings(), (async () => html.clone()) as typeof fetch);
  await assert.rejects(t.tree(), LoginRequired);
});

test("an HTML error page is a plain error, not a login", async () => {
  const html = (status: number) => new Response("<html>", { status, headers: { "content-type": "text/html" } });
  assert.ok(!isLoginPage(html(502)));
  assert.ok(!isLoginPage(html(401)));
  const t = httpTransport(settings(), (async () => html(502)) as typeof fetch);
  await assert.rejects(t.tree(), HttpError);
});

test("a redirect inside the server is followed, one to a foreign login is a login", async () => {
  const redirect = { type: "opaqueredirect", status: 0, headers: new Headers(), ok: false } as unknown as Response;
  let calls = 0;
  const same = httpTransport(settings(), (async (_u: unknown, i?: RequestInit) => {
    calls++;
    return i?.redirect === "manual" ? redirect : json({ rev: 1, nodes: [] });
  }) as typeof fetch);
  await same.tree();
  assert.equal(calls, 2);
  const foreign = httpTransport(settings(), (async (_u: unknown, i?: RequestInit) => {
    if (i?.redirect === "manual") return redirect;
    throw new TypeError("Failed to fetch");
  }) as typeof fetch);
  await assert.rejects(foreign.tree(), LoginRequired);
});
