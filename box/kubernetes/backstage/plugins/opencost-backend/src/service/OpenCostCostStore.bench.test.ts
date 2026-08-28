/**
 * Benchmark test for OpenCostCostStore batch operations.
 *
 * Measures query count and elapsed time for:
 *   - insertDailyCosts (batch pod upsert + batch daily cost insert)
 *   - aggregateMonth   (single INSERT INTO...SELECT)
 *
 * Run:
 *   npx jest --config jest.bench.config.js OpenCostCostStore.bench.test.ts
 */
import knex, { Knex } from 'knex';
import { OpenCostCostStore, DailyCostItem } from './OpenCostCostStore';
import { ControllerFilterPresets, validatePresetInput } from './ControllerFilterPresets';

/* ────────────────────────────────
 *  Helpers
 * ──────────────────────────────── */

/** Wrap a Knex instance to count raw/builder queries. */
function withQueryCounter(db: Knex): { db: Knex; counter: { count: number } } {
  const counter = { count: 0 };
  db.on('query', () => {
    counter.count++;
  });
  return { db, counter };
}

/** Generate N fake DailyCostItem records with unique pods. */
function generateItems(n: number): DailyCostItem[] {
  const items: DailyCostItem[] = [];
  for (let i = 0; i < n; i++) {
    items.push({
      namespace: `ns-${i % 10}`,
      controllerKind: 'Deployment',
      controller: `deploy-${i % 50}`,
      pod: `pod-${i}`,
      cpuCost: Math.random() * 10,
      ramCost: Math.random() * 5,
      gpuCost: 0,
      pvCost: Math.random() * 2,
      networkCost: Math.random() * 1,
      totalCost: Math.random() * 20,
      carbonCost: Math.random() * 0.5,
    });
  }
  return items;
}

/* ────────────────────────────────
 *  Test suite
 * ──────────────────────────────── */

describe('OpenCostCostStore batch performance', () => {
  let db: Knex;
  let counter: { count: number };
  let store: OpenCostCostStore;

  beforeAll(async () => {
    const rawDb = knex({
      client: 'better-sqlite3',
      connection: { filename: ':memory:' },
      useNullAsDefault: true,
    });
    ({ db, counter } = withQueryCounter(rawDb));

    store = await OpenCostCostStore.create({ database: db });

    // Seed a cluster
    await store.ensureCluster('bench-cluster', 'Bench Cluster');
  });

  afterAll(async () => {
    await db.destroy();
  });

  beforeEach(() => {
    counter.count = 0;
  });

  /* ─── insertDailyCosts ─── */

  test.each([100, 500, 1000])(
    'insertDailyCosts — %i pods',
    async (podCount) => {
      const clusterId = (await store.getClusterId('bench-cluster'))!;
      const items = generateItems(podCount);
      const date = '2025-01-15';

      // Clean previous run data
      await db('opencost_daily_costs').where({ cluster_id: clusterId, date }).del();

      counter.count = 0;
      const start = performance.now();

      await store.insertDailyCosts(clusterId, date, items);

      const elapsed = performance.now() - start;
      const queries = counter.count;

      // Batch approach: ceil(pods/100) pod upserts + 1 fetch + ceil(items/50) cost upserts
      const expectedMax = Math.ceil(podCount / 100) + 1 + Math.ceil(podCount / 50);

      console.log(
        `  insertDailyCosts(${podCount} pods): ${queries} queries, ${elapsed.toFixed(1)}ms` +
        ` (batch upper bound: ${expectedMax})`,
      );

      // Assert query count is within batch bounds (not N+M individual queries)
      expect(queries).toBeLessThanOrEqual(expectedMax);
      // Sanity: old approach would be > podCount queries
      expect(queries).toBeLessThan(podCount);
    },
    30_000,
  );

  /* ─── aggregateMonth ─── */

  test.each([100, 500, 1000])(
    'aggregateMonth — %i pods across 28 days',
    async (podCount) => {
      const clusterId = (await store.getClusterId('bench-cluster'))!;

      // Seed 28 days of daily data
      for (let day = 1; day <= 28; day++) {
        const date = `2025-02-${String(day).padStart(2, '0')}`;
        const items = generateItems(podCount);
        await store.insertDailyCosts(clusterId, date, items);
      }

      // Clear monthly table
      await db('opencost_monthly_summaries')
        .where({ cluster_id: clusterId, year: 2025, month: 2 })
        .del();

      counter.count = 0;
      const start = performance.now();

      const result = await store.aggregateMonth(clusterId, 2025, 2);

      const elapsed = performance.now() - start;
      const queries = counter.count;

      console.log(
        `  aggregateMonth(${podCount} pods × 28 days): ${queries} queries, ${elapsed.toFixed(1)}ms` +
        ` → ${result} pods aggregated`,
      );

      // Single INSERT...SELECT + 1 COUNT = 2 queries total
      expect(queries).toBe(2);
      expect(result).toBe(podCount);
    },
    120_000,
  );

  /* ─── getMonthlyTotals ─── */

  test('getMonthlyTotals — one aggregated row from monthly_summaries', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    // 2025-02 was aggregated by the previous test
    const perPod = await store.getMonthlySummary(clusterId, 2025, 2);
    expect(perPod.length).toBeGreaterThan(0);

    counter.count = 0;
    const start = performance.now();
    const totals = await store.getMonthlyTotals(clusterId, 2025, 2);
    const elapsed = performance.now() - start;
    console.log(`  getMonthlyTotals(monthly, ${perPod.length} pods): ${counter.count} queries, ${elapsed.toFixed(1)}ms`);

    expect(counter.count).toBe(1);
    expect(totals.source).toBe('monthly');
    expect(totals.podCount).toBe(perPod.length);
    expect(totals.daysCovered).toBe(perPod[0].daysCovered);
    const expected = perPod.reduce((s, r) => s + r.totalCost, 0);
    expect(totals.totalCost).toBeCloseTo(expected, 4);
  });

  test('getMonthlyTotals — falls back to daily_costs when month not aggregated', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    for (let day = 1; day <= 5; day++) {
      await store.insertDailyCosts(clusterId, `2025-03-${String(day).padStart(2, '0')}`, generateItems(200));
    }
    const { rows, daysCovered } = await store.aggregateMonthOnTheFly(clusterId, 2025, 3);

    counter.count = 0;
    const totals = await store.getMonthlyTotals(clusterId, 2025, 3);

    // 1 miss on monthly_summaries + 1 aggregate over daily_costs
    expect(counter.count).toBe(2);
    expect(totals.source).toBe('daily');
    expect(totals.daysCovered).toBe(daysCovered);
    expect(totals.podCount).toBe(rows.length);
    expect(totals.totalCost).toBeCloseTo(rows.reduce((s, r) => s + r.totalCost, 0), 4);
  });

  test('getMonthlyTotals — controller filter keeps cluster-wide daysCovered', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    const filtered = await store.getMonthlyTotals(clusterId, 2025, 3, { controllers: ['deploy-1'] });
    const { rows } = await store.aggregateMonthOnTheFly(clusterId, 2025, 3, { controllers: ['deploy-1'] });

    expect(filtered.source).toBe('daily');
    expect(filtered.daysCovered).toBe(5);
    expect(filtered.podCount).toBe(rows.length);
    expect(filtered.totalCost).toBeCloseTo(rows.reduce((s, r) => s + r.totalCost, 0), 4);
  });

  test('controller LIKE patterns — ORed, ANDed with explicit list', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    // generateItems assigns controller deploy-(i % 50), so deploy-1% matches deploy-1, deploy-10..19
    const { rows } = await store.aggregateMonthOnTheFly(clusterId, 2025, 3, { patterns: ['deploy-1%'] });
    const names = new Set(rows.map(r => r.controller));
    expect(names.size).toBe(11);
    for (const n of names) expect(n!.startsWith('deploy-1')).toBe(true);

    const two = await store.aggregateMonthOnTheFly(clusterId, 2025, 3, { patterns: ['deploy-4_', 'deploy-2'] });
    const twoNames = new Set(two.rows.map(r => r.controller));
    expect(twoNames.size).toBe(11);
    expect(twoNames.has('deploy-2')).toBe(true);
    expect(twoNames.has('deploy-40')).toBe(true);

    const both = await store.aggregateMonthOnTheFly(clusterId, 2025, 3, { patterns: ['deploy-1%'], controllers: ['deploy-12', 'deploy-3'] });
    expect(new Set(both.rows.map(r => r.controller))).toEqual(new Set(['deploy-12']));

    const totals = await store.getMonthlyTotals(clusterId, 2025, 3, { patterns: ['deploy-1%'] });
    expect(totals.podCount).toBe(rows.length);
    expect(totals.totalCost).toBeCloseTo(rows.reduce((s, r) => s + r.totalCost, 0), 4);

    const daily = await store.getDailySummary(clusterId, 2025, 3, { patterns: ['deploy-1%'] });
    expect(daily.length).toBe(5);
    expect(daily.reduce((s, d) => s + d.totalCost, 0)).toBeCloseTo(totals.totalCost, 4);

    const pods = await store.getPodsForDate(clusterId, '2025-03-01', { patterns: ['deploy-1%'] });
    expect(pods.length).toBe(rows.length);

    const ctrls = await store.getControllers(clusterId, 2025, 3, { patterns: ['deploy-1%'] });
    expect(ctrls.map(c => c.controller).sort()).toEqual(Array.from(names).sort());
  });

  test('controller filter presets — CRUD and request resolution', async () => {
    const presets = new ControllerFilterPresets(store);
    expect(await presets.list()).toEqual([]);

    const v = validatePresetInput({ title: 'Vendor X', patterns: ['deploy-1%', ' deploy-2 ', 'deploy-1%', ''], clusters: ['bench-cluster'], name: 'vendor-x' });
    expect(v.ok).toBe(true);
    if (!v.ok) throw new Error(v.error);
    expect(v.value.patterns).toEqual(['deploy-1%', 'deploy-2']);

    const created = await presets.save(v.value);
    expect(created.name).toBe('vendor-x');
    expect(created.clusters).toEqual(['bench-cluster']);

    const updated = await presets.save({ ...v.value, title: 'Vendor X (renamed)', clusters: null });
    expect(updated.title).toBe('Vendor X (renamed)');
    expect(updated.clusters).toBeNull();
    expect((await presets.list()).length).toBe(1);

    const resolved = await presets.resolve('vendor-x', ['deploy-12'], 'bench-cluster');
    expect(resolved.filter).toEqual({ controllers: ['deploy-12'], patterns: ['deploy-1%', 'deploy-2'] });
    expect((await presets.resolve('nope', undefined, 'bench-cluster')).error).toMatch(/Unknown/);

    await presets.save({ ...v.value, clusters: ['other'] });
    expect((await presets.resolve('vendor-x', undefined, 'bench-cluster')).error).toMatch(/not enabled/);

    expect(await presets.remove('vendor-x')).toBe(true);
    expect(await presets.remove('vendor-x')).toBe(false);

    expect(validatePresetInput({ name: 'Bad Name', patterns: ['x'] }).ok).toBe(false);
    expect(validatePresetInput({ name: 'ok', patterns: [] }).ok).toBe(false);
    expect(validatePresetInput({ name: 'ok', patterns: 'x' }).ok).toBe(false);
  });

  test('getMonthlyTotals — empty month reports source none', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    const totals = await store.getMonthlyTotals(clusterId, 2019, 1);
    expect(totals.source).toBe('none');
    expect(totals.totalCost).toBe(0);
  });

  test('getControllers — year-wide query without month', async () => {
    const clusterId = (await store.getClusterId('bench-cluster'))!;
    const year = await store.getControllers(clusterId, 2025);
    const feb = await store.getControllers(clusterId, 2025, 2);
    expect(year.length).toBeGreaterThanOrEqual(feb.length);
    expect(year.map(c => c.controller)).toEqual(expect.arrayContaining(feb.map(c => c.controller)));
  });
});
