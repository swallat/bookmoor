// Fractional index positions. Mirrors crates/bookmoor-core/src/position.rs;
// both run the shared vectors in testvectors/position.json.

const DIGITS = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
const BASE = DIGITS.length;

export class PositionError extends Error {}

export function isValid(p: string): boolean {
  if (p.length === 0 || p.endsWith("0")) return false;
  for (const c of p) if (!DIGITS.includes(c)) return false;
  return true;
}

function digit(c: string | undefined): number {
  return c === undefined ? 0 : DIGITS.indexOf(c);
}

/** A position strictly between `before` and `after` (null = no neighbour). */
export function between(before: string | null, after: string | null): string {
  for (const p of [before, after]) {
    if (p !== null && !isValid(p)) throw new PositionError(`invalid position ${JSON.stringify(p)}`);
  }
  if (before !== null && after !== null && before >= after) {
    throw new PositionError("lower bound is not smaller than upper bound");
  }
  return midpoint(before ?? "", after);
}

// `a` may be empty (zero), `b === null` means infinity. Requires a < b.
function midpoint(a: string, b: string | null): string {
  if (b !== null) {
    let n = 0;
    while (n < b.length && (a[n] ?? "0") === b[n]) n++;
    if (n > 0) return b.slice(0, n) + midpoint(a.slice(n), b.slice(n));
  }
  const da = digit(a[0]);
  const db = b === null ? BASE : digit(b[0]);
  if (db - da > 1) return DIGITS[Math.ceil((da + db) / 2)]!;
  if (b !== null && b.length > 1) return b.slice(0, 1);
  return DIGITS[da]! + midpoint(a.slice(1), null);
}

/** Total order for siblings: by position, ties broken by id. */
export function compareSiblings(a: [string, string], b: [string, string]): number {
  return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : a[1] < b[1] ? -1 : a[1] > b[1] ? 1 : 0;
}
