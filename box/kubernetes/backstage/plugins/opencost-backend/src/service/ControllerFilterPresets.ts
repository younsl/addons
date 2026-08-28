import {
  ControllerFilter,
  ControllerFilterPreset,
  ControllerFilterPresetInput,
  OpenCostCostStore,
} from './OpenCostCostStore';

const NAME_RE = /^[a-z0-9][a-z0-9_-]{0,63}$/;
const MAX_PATTERNS = 50;
const MAX_PATTERN_LENGTH = 253;

export type ValidationResult =
  | { ok: true; value: ControllerFilterPresetInput }
  | { ok: false; error: string };

/**
 * Validate a preset payload from the UI. Names are URL-safe slugs so they can be
 * passed as `?filter=` and used in deep links. Patterns are kept verbatim (SQL LIKE).
 */
export function validatePresetInput(body: unknown): ValidationResult {
  if (!body || typeof body !== 'object') return { ok: false, error: 'Body must be a JSON object' };
  const b = body as Record<string, unknown>;

  const name = typeof b.name === 'string' ? b.name.trim() : '';
  if (!NAME_RE.test(name)) {
    return { ok: false, error: 'name must match ^[a-z0-9][a-z0-9_-]{0,63}$' };
  }

  const title = typeof b.title === 'string' && b.title.trim() ? b.title.trim() : name;
  if (title.length > 100) return { ok: false, error: 'title must be 100 characters or fewer' };

  const description = typeof b.description === 'string' && b.description.trim()
    ? b.description.trim()
    : null;

  if (!Array.isArray(b.patterns)) return { ok: false, error: 'patterns must be an array of strings' };
  const patterns = Array.from(new Set(
    b.patterns.filter((p): p is string => typeof p === 'string').map(p => p.trim()).filter(Boolean),
  ));
  if (patterns.length === 0) return { ok: false, error: 'patterns must contain at least one non-empty pattern' };
  if (patterns.length > MAX_PATTERNS) return { ok: false, error: `patterns must contain at most ${MAX_PATTERNS} entries` };
  if (patterns.some(p => p.length > MAX_PATTERN_LENGTH)) {
    return { ok: false, error: `each pattern must be ${MAX_PATTERN_LENGTH} characters or fewer` };
  }

  let clusters: string[] | null = null;
  if (b.clusters !== undefined && b.clusters !== null) {
    if (!Array.isArray(b.clusters)) return { ok: false, error: 'clusters must be an array of strings' };
    const list = Array.from(new Set(
      b.clusters.filter((c): c is string => typeof c === 'string').map(c => c.trim()).filter(Boolean),
    ));
    clusters = list.length > 0 ? list : null;
  }

  return { ok: true, value: { name, title, description, patterns, clusters } };
}

/**
 * Resolves request filter parameters against the presets stored in the database.
 */
export class ControllerFilterPresets {
  constructor(private readonly store: OpenCostCostStore) {}

  list(): Promise<ControllerFilterPreset[]> {
    return this.store.listFilterPresets();
  }

  get(name: string): Promise<ControllerFilterPreset | undefined> {
    return this.store.getFilterPreset(name);
  }

  save(input: ControllerFilterPresetInput): Promise<ControllerFilterPreset> {
    return this.store.upsertFilterPreset(input);
  }

  remove(name: string): Promise<boolean> {
    return this.store.deleteFilterPreset(name);
  }

  /**
   * Build the store filter for a request. `presetName` must exist when given, and the
   * preset must not exclude `cluster`. Returns an error message on rejection so the
   * router can answer 400 without throwing.
   */
  async resolve(
    presetName: string | undefined,
    controllers: string[] | undefined,
    cluster: string,
  ): Promise<{ filter?: ControllerFilter; error?: string }> {
    const filter: ControllerFilter = {};
    if (controllers && controllers.length > 0) filter.controllers = controllers;
    if (presetName) {
      const preset = await this.store.getFilterPreset(presetName);
      if (!preset) return { error: `Unknown controller filter: ${presetName}` };
      if (preset.clusters && !preset.clusters.includes(cluster)) {
        return { error: `Controller filter '${presetName}' is not enabled for cluster '${cluster}'` };
      }
      filter.patterns = preset.patterns;
    }
    return { filter: Object.keys(filter).length > 0 ? filter : undefined };
  }
}
