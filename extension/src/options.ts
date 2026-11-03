import { loadSettings, originPattern, saveSettings, type HeaderEntry } from "./settings";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;

function headerRow(entry: HeaderEntry = { name: "", value: "" }): HTMLDivElement {
  const row = document.createElement("div");
  row.className = "row";
  const name = Object.assign(document.createElement("input"), { placeholder: "Header name", value: entry.name });
  const value = Object.assign(document.createElement("input"), {
    type: "password",
    placeholder: "Header value",
    value: entry.value,
  });
  const remove = Object.assign(document.createElement("button"), { type: "button", textContent: "Remove" });
  remove.addEventListener("click", () => row.remove());
  row.append(name, value, remove);
  return row;
}

function readHeaders(): HeaderEntry[] {
  return [...$("headers").querySelectorAll<HTMLDivElement>(".row")]
    .map((row) => {
      const [name, value] = row.querySelectorAll("input");
      return { name: name?.value ?? "", value: value?.value ?? "" };
    })
    .filter((h) => h.name.trim() !== "");
}

async function init(): Promise<void> {
  const s = await loadSettings();
  $<HTMLInputElement>("url").value = s.serverUrl;
  $<HTMLInputElement>("token").value = s.token;
  for (const h of s.headers) $("headers").append(headerRow(h));
  $("add").addEventListener("click", () => $("headers").append(headerRow()));
  $("save").addEventListener("click", async () => {
    const status = $("status");
    try {
      const serverUrl = $<HTMLInputElement>("url").value.trim();
      // Must run inside the click handler: permission requests need a user gesture.
      const granted = await chrome.permissions.request({ origins: [originPattern(serverUrl)] });
      if (!granted) throw new Error("Permission for the server origin was denied.");
      await saveSettings({ serverUrl, token: $<HTMLInputElement>("token").value, headers: readHeaders() });
      status.textContent = "Saved.";
    } catch (e) {
      status.textContent = `Error: ${e instanceof Error ? e.message : String(e)}`;
    }
  });
}

void init();
