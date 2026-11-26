export const GIB = 1024 ** 3;

/** Bytes as a GiB number without trailing zeros: 4 GiB -> "4", 4.5 GiB -> "4.5". */
export function formatGiB(bytes: number): string {
  const value = Number(bytes);
  if (!Number.isFinite(value) || value <= 0) return '0';
  return String(Number((value / GIB).toFixed(4)));
}

/** A decimal GiB amount typed by the user, in bytes; `null` when it is not a plain decimal. */
export function parseGiB(text: string): number | null {
  const match = /^(\d+)(?:\.(\d*))?$/.exec(text.trim());
  if (!match) return null;
  const whole = Number(match[1]);
  const fractionText = (match[2] ?? '').slice(0, 9);
  const fraction = fractionText ? Number(fractionText) / 10 ** fractionText.length : 0;
  return Math.round((whole + fraction) * GIB);
}
