import type { MessageKey } from "@/lib/i18n";

// The artifact listing's `filter` values: each keeps what one Statistics panel
// counts, so a panel's drill-down lists exactly the number it shows.
export const ARTIFACT_FILTERS = ["labeled", "scanned", "clean", "vulnerable", "licensed", "broken"] as const;
export type ArtifactFilter = (typeof ARTIFACT_FILTERS)[number];

// parseArtifactFilter reads the Artifacts tab's `filter` search parameter,
// dropping anything the server would reject.
export function parseArtifactFilter(value: unknown): ArtifactFilter | undefined {
  return ARTIFACT_FILTERS.find((f) => f === value);
}

// Each filter is named by the title of the panel it drills down from.
export const ARTIFACT_FILTER_LABEL: Record<ArtifactFilter, MessageKey> = {
  labeled: "repo.stat-labeled",
  scanned: "repo.stat-scanned",
  clean: "repo.clean-ratio",
  vulnerable: "repo.stat-vulnerable",
  licensed: "repo.stat-licenses",
  broken: "repo.stat-broken",
};

// Filters the server applies over the Statistics sample (the 500 most recently
// accessed artifacts) rather than the whole repository, matching their panels.
export const SAMPLED_ARTIFACT_FILTERS: ReadonlySet<ArtifactFilter> = new Set<ArtifactFilter>([
  "scanned", "clean", "vulnerable", "licensed", "broken",
]);
