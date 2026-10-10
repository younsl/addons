const BYTE_UNIT = 1024;
const FILE_SIZE_UNITS = ["KB", "MB", "GB", "TB"] as const;

export function formatFileSize(bytes: number): string {
  if (bytes < BYTE_UNIT) return `${bytes} B`;

  let value = bytes / BYTE_UNIT;
  let unitIndex = 0;

  while (value >= BYTE_UNIT && unitIndex < FILE_SIZE_UNITS.length - 1) {
    value /= BYTE_UNIT;
    unitIndex += 1;
  }

  return `${value.toFixed(1)} ${FILE_SIZE_UNITS[unitIndex]}`;
}
