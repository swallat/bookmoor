// Test-only (copied into dist-e2e*): runs a WebExtension API call on request,
// so a WebDriver client can use it without script access to extension globals.
// Request: set document.body.dataset.cmd = JSON {path, args} and dispatch an
// "e2e-cmd" event; the JSON result (or {error}) lands in dataset.result.
document.addEventListener("e2e-cmd", async () => {
  const b = document.body;
  delete b.dataset.result;
  let out;
  try {
    const { path, args } = JSON.parse(b.dataset.cmd);
    const parts = path.split(".");
    const name = parts.pop();
    const target = parts.reduce((o, k) => o[k], chrome);
    out = { value: (await target[name](...args)) ?? null };
  } catch (e) {
    out = { error: String(e) };
  }
  b.dataset.result = JSON.stringify(out);
});
