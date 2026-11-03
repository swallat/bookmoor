# bookmoor logo

Concept: a bookmark ribbon with a mooring eye. A bookmark gets made fast (moored)
in one place, which is what the sync does.

| File | Use |
|---|---|
| `bookmoor-icon.svg` | Master mark, for 48 px and larger (README, store listing, favicon) |
| `bookmoor-icon-small.svg` | Reduced variant for 16/32 px (no inner ring, bigger eye) |
| `render.js` | Renders the PNG sizes (`NODE_PATH=$(npm root -g) node render.js in.svg out-prefix`, needs Playwright) |

The extension uses `extension/icons/icon-{16,32}.png` (small variant) and
`icon-{48,128}.png` (master).

Palette (variant C, "Nebelblau"): blue `#1D4E89`, white `#FFFFFF`, yellow `#FFB703`.
Alternatives A, B, D, E are in `variants/` (overview in `variants/overview.png`).

## Review (impeccable.style criteria)

impeccable is an agent skill and a detector for web UI. It has no check for
SVG logos, so the review applied its critique criteria by hand: hierarchy,
clarity, craft, and the "AI slop" tells.

- Hierarchy: one focal point, the yellow eye on the white ribbon. Fixed: the first
  draft (green palette, since replaced) had a thin ring that blended into the tile at 16 px.
- Clarity: reads as "bookmark" at every size. The mooring idea is carried by the
  eye only, so it stays quiet rather than literal.
- Craft: first draft had a 4 px tile inset, which blurred the edge at 16 px.
  Now 2 px in the master and full-bleed in the small variant. The small variant
  drops detail that cannot survive 16 px.
- Slop tells: no gradients, glow, drop shadows, or generic sparkle. Three flat colors.
- Contrast: white on blue is about 8:1. Yellow on white is the weak pair (about 1.6:1), so the master
  rings the eye in blue. The small variant drops the ring and relies on hue and size,
  but yellow on blue stays clearly visible at 16 px because the eye is flanked by blue.
- Not done: a wordmark lockup. It needs a font decision and outlined text.
