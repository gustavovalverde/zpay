export const zecFormatter = new Intl.NumberFormat("en-US", {
  maximumFractionDigits: 8,
  minimumFractionDigits: 0
});

export const integerFormatter = new Intl.NumberFormat("en-US");

export function formatZec(zat: number): string {
  return zecFormatter.format(zat / 100_000_000);
}

export function truncateMiddle(text: string, startCount: number, endCount: number): string {
  if (text.length <= startCount + endCount + 3) {
    return text;
  }
  return `${text.slice(0, startCount)}…${text.slice(-endCount)}`;
}
