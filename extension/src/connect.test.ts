import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { connectUrl, deviceName, newState, tokenFromRedirect } from "./connect.ts";

describe("connect", () => {
  it("builds the page URL below a path prefix", () => {
    const u = new URL(connectUrl("https://h.example/bookmoor?x=1", "https://abc.chromiumapp.org/", "st", "Chrome on Linux"));
    assert.equal(u.pathname, "/bookmoor/");
    assert.equal(u.searchParams.get("connect"), "https://abc.chromiumapp.org/");
    assert.equal(u.searchParams.get("state"), "st");
    assert.equal(u.searchParams.get("name"), "Chrome on Linux");
    assert.equal(u.searchParams.get("x"), null);
  });

  it("reads the token from the fragment and checks the state", () => {
    assert.equal(tokenFromRedirect("https://abc.chromiumapp.org/#token=bkm_a_b&state=s1", "s1"), "bkm_a_b");
    assert.throws(() => tokenFromRedirect("https://abc.chromiumapp.org/#token=bkm_a_b&state=other", "s1"), /did not match/);
    assert.throws(() => tokenFromRedirect("https://abc.chromiumapp.org/#state=s1", "s1"), /not return a token/);
    assert.throws(() => tokenFromRedirect("https://abc.chromiumapp.org/?token=x&state=s1", "s1"), /did not match/);
  });

  it("names the device", () => {
    assert.equal(deviceName("Mozilla/5.0 (X11; Linux x86_64) Gecko/20100101 Firefox/130.0", "Linux x86_64"), "Firefox on Linux");
    assert.equal(deviceName("Mozilla/5.0 Chrome/130 Safari/537", "Win32"), "Chrome on Windows");
    assert.equal(deviceName("curl", ""), "Browser");
  });

  it("makes a random state", () => {
    assert.match(newState(), /^[0-9a-f]{32}$/);
    assert.notEqual(newState(), newState());
  });
});
