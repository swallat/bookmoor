import assert from "node:assert/strict";
import { test } from "node:test";
import { HttpError, LoginRequired } from "./flow.ts";
import { httpTransport, isLoginPage } from "./transport.ts";
import type { Settings } from "./settings.ts";

const settings = (sso: boolean): Settings => ({ serverUrl: "https://b.example", token: "t", headers: [], query: [], sso });

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

test("without sso the request carries no cookies and a redirect is a plain error", async () => {
  let init: RequestInit | undefined;
  const t = httpTransport(settings(false), (async (_u: unknown, i?: RequestInit) => {
    init = i;
    return json({ error: "nope" }, 500);
  }) as typeof fetch);
  await assert.rejects(t.tree(), HttpError);
  assert.equal(init?.credentials, undefined);
  assert.equal(init?.redirect, undefined);
});

test("with sso cookies are sent and redirects are not followed", async () => {
  let init: RequestInit | undefined;
  const t = httpTransport(settings(true), (async (_u: unknown, i?: RequestInit) => {
    init = i;
    return json({ rev: 1, nodes: [] });
  }) as typeof fetch);
  await t.tree();
  assert.equal(init?.credentials, "include");
  assert.equal(init?.redirect, "manual");
});

test("with sso a redirect or an HTML page means login required", async () => {
  const redirect = { type: "opaqueredirect" as const, status: 0, headers: new Headers() };
  assert.ok(isLoginPage(redirect));
  assert.ok(isLoginPage({ type: "basic" as const, status: 302, headers: new Headers() }));
  const html = new Response("<html>", { status: 200, headers: { "content-type": "text/html; charset=utf-8" } });
  assert.ok(isLoginPage(html));
  assert.ok(!isLoginPage(json({})));
  const t = httpTransport(settings(true), (async () => html.clone()) as typeof fetch);
  await assert.rejects(t.tree(), LoginRequired);
});

test("without sso an HTML answer is not treated as a login", async () => {
  const t = httpTransport(settings(false), (async () => new Response("<html>", { status: 502, headers: { "content-type": "text/html" } })) as typeof fetch);
  await assert.rejects(t.tree(), HttpError);
});
