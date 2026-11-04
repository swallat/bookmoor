import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { describe, it } from "node:test";

describe("manifest", () => {
  it("shows the popup with the sync button when the toolbar icon is clicked", () => {
    const m = JSON.parse(readFileSync(new URL("../manifest.json", import.meta.url), "utf8"));
    assert.equal(m.action.default_popup, "popup.html");
    assert.ok(existsSync(new URL("./popup.html", import.meta.url)));
    assert.match(readFileSync(new URL("./popup.html", import.meta.url), "utf8"), /id="syncnow"/);
  });
});
