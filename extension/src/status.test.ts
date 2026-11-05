import assert from "node:assert/strict";
import { test } from "node:test";
import { HttpError, LoginRequired } from "./flow.ts";
import { describe } from "./status.ts";

test("describe explains the common failures", () => {
  assert.match(describe("ok", undefined), /succeeded/);
  assert.match(describe("auth", new HttpError(401)), /rejected the token/);
  assert.match(
    describe("auth", new HttpError(403, "this token has no device and cannot sync: create a device token with scope sync")),
    /no device.*scope sync/,
  );
  assert.match(describe("auth", new HttpError(403, "forbidden")), /scope sync/);
  assert.match(describe("retry", new TypeError("Failed to fetch")), /Could not reach the server.*Failed to fetch/);
});

test("describe tells the user to sign in when the proxy login is in the way", () => {
  assert.match(describe("login", new LoginRequired()), /Sign in again/);
});
