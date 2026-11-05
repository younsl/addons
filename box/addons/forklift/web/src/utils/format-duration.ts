const MS_PER_SECOND = 1000;

// formatMilliseconds renders a duration with a dynamic unit: milliseconds under
// one second, seconds (1 decimal) from there up.
export function formatMilliseconds(ms: number): string {
  if (ms < MS_PER_SECOND) return `${ms}ms`;
  return `${(ms / MS_PER_SECOND).toFixed(1)}s`;
}
