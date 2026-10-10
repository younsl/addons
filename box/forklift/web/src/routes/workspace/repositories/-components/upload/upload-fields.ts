// validClassifier mirrors the server's safe-segment rule with a conventional
// Maven allowlist: empty is allowed (the classifier is optional); otherwise
// letters, digits, dot, underscore and hyphen only, no leading dot and no "..".
//
// The path traversal guard is the reason for the last two: a classifier lands
// in the stored path, and ".." there would climb out of the repository.
export function validClassifier(value: string): boolean {
  const classifier = value.trim();

  if (classifier === "") return true;

  return (
    /^[A-Za-z0-9._-]+$/.test(classifier) &&
    !classifier.startsWith(".") &&
    !classifier.includes("..")
  );
}

// inferExtension takes the Maven packaging from the filename. ".tar.gz" is
// special-cased because the last dot alone would yield "gz", which is the
// compression rather than the packaging.
export function inferExtension(filename: string): string {
  const base = filename.toLowerCase();

  if (base.endsWith(".tar.gz")) return "tar.gz";

  const dot = base.lastIndexOf(".");

  return dot >= 0 ? base.slice(dot + 1) : "";
}
