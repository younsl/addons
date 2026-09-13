import { Router, json as jsonBody } from 'express';
import { HttpAuthService, LoggerService } from '@backstage/backend-plugin-api';
import { OpenCostService } from './OpenCostService';
import { OpenCostCostStore } from './OpenCostCostStore';
import { OpenCostCollector } from './OpenCostCollector';
import { ControllerFilterPresets, validatePresetInput } from './ControllerFilterPresets';

export interface RouterOptions {
  service: OpenCostService;
  costStore: OpenCostCostStore;
  collector: OpenCostCollector;
  presets: ControllerFilterPresets;
  httpAuth: HttpAuthService;
  logger: LoggerService;
}

export async function createRouter(options: RouterOptions): Promise<Router> {
  const { service, costStore, collector, presets, httpAuth, logger } = options;

  /** Entity ref of the calling Backstage user, or null for service or unauthenticated callers. */
  const actorOf = async (req: unknown): Promise<string | null> => {
    try {
      // Express's typed Request<{name}> is not assignable to the untyped one HttpAuthService expects
      const creds = await httpAuth.credentials(req as any, { allow: ['user', 'service', 'none'] });
      const p = creds.principal as { type: string; userEntityRef?: string };
      return p.type === 'user' && p.userEntityRef ? p.userEntityRef : null;
    } catch {
      return null;
    }
  };

  const router = Router();
  router.use(jsonBody({ limit: '64kb' }));

  /** Parse `controllers` (comma list) and `filter` (preset name) into a store filter. */
  const parseFilter = async (req: { query: Record<string, unknown> }, cluster: string) => {
    const controllersParam = req.query.controllers as string | undefined;
    const controllers = controllersParam ? controllersParam.split(',').filter(Boolean) : undefined;
    const presetName = (req.query.filter as string | undefined) || undefined;
    return presets.resolve(presetName, controllers, cluster);
  };

  // Log response time for all routes except /health
  router.use((req, res, next) => {
    if (req.path === '/health') return next();
    const start = Date.now();
    res.on('finish', () => {
      const ms = Date.now() - start;
      logger.info(`${req.method} ${req.path} ${res.statusCode} ${ms}ms`, {
        path: req.path,
        query: req.query,
        status: res.statusCode,
        durationMs: ms,
      });
    });
    next();
  });

  router.get('/health', (_, res) => {
    res.json({ status: 'ok' });
  });

  router.get('/config', (_, res) => {
    res.json({
      timezone: collector.timezone,
      dailyCollectorCron: collector.dailyCronLocal,
    });
  });

  /**
   * Controller filter presets, managed from the OpenCost UI.
   *   GET    /filters          list
   *   PUT    /filters/:name    create or replace
   *   DELETE /filters/:name    delete
   */
  router.get('/filters', async (_req, res) => {
    try {
      res.json({ data: await presets.list() });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error listing controller filters: ${msg}`);
      res.status(500).json({ message: 'Internal error listing controller filters' });
    }
  });

  /**
   * POST /filters/preview  { cluster, year, month, patterns[] }
   * Dry-run a pattern list against stored data so the editor can show what a preset
   * would match before it is saved. Nothing is persisted.
   */
  router.post('/filters/preview', async (req, res) => {
    const body = (req.body ?? {}) as Record<string, unknown>;
    const cluster = typeof body.cluster === 'string' ? body.cluster : undefined;
    const year = Number(body.year);
    const month = Number(body.month);
    const validated = validatePresetInput({ name: 'preview', patterns: body.patterns });
    if (!cluster || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, year, month (1-12), patterns[]' });
      return;
    }
    if (!validated.ok) {
      res.status(400).json({ message: validated.error });
      return;
    }
    try {
      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: { controllers: [], controllerCount: 0, podCount: 0, totalCost: 0, daysCovered: 0, samplePods: [] } });
        return;
      }
      const filter = { patterns: validated.value.patterns };
      const [controllers, totals, { rows }] = await Promise.all([
        costStore.getControllers(clusterId, year, month, filter),
        costStore.getMonthlyTotals(clusterId, year, month, filter),
        costStore.aggregateMonthOnTheFly(clusterId, year, month, filter),
      ]);
      const CONTROLLER_CAP = 100;
      const SAMPLE_CAP = 20;
      res.json({
        data: {
          controllers: controllers.slice(0, CONTROLLER_CAP),
          controllerCount: controllers.length,
          podCount: totals.podCount,
          totalCost: totals.totalCost,
          daysCovered: totals.daysCovered,
          samplePods: rows.slice(0, SAMPLE_CAP).map(r => ({
            namespace: r.namespace,
            controllerKind: r.controllerKind,
            controller: r.controller,
            pod: r.pod,
            totalCost: r.totalCost,
          })),
        },
      });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error previewing controller filter for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error previewing controller filter' });
    }
  });

  router.put('/filters/:name', async (req, res) => {
    const validated = validatePresetInput({ ...(req.body ?? {}), name: req.params.name });
    if (!validated.ok) {
      res.status(400).json({ message: validated.error });
      return;
    }
    try {
      const existed = !!(await presets.get(validated.value.name));
      const actor = await actorOf(req);
      const saved = await presets.save(validated.value, actor);
      logger.info(`Controller filter '${saved.name}' ${existed ? 'updated' : 'created'} by ${actor ?? 'unknown'} (${saved.patterns.length} pattern(s))`);
      res.status(existed ? 200 : 201).json({ data: saved });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error saving controller filter '${req.params.name}': ${msg}`);
      res.status(500).json({ message: 'Internal error saving controller filter' });
    }
  });

  router.delete('/filters/:name', async (req, res) => {
    try {
      const removed = await presets.remove(req.params.name);
      if (!removed) {
        res.status(404).json({ message: `Unknown controller filter: ${req.params.name}` });
        return;
      }
      logger.info(`Controller filter '${req.params.name}' deleted by ${(await actorOf(req)) ?? 'unknown'}`);
      res.status(204).end();
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error deleting controller filter '${req.params.name}': ${msg}`);
      res.status(500).json({ message: 'Internal error deleting controller filter' });
    }
  });

  router.get('/clusters/status', async (_req, res) => {
    const statuses = await service.checkClustersStatus();
    res.json({ data: statuses });
  });

  router.get('/allocation', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    if (!cluster) {
      res.status(400).json({ message: 'Missing required query parameter: cluster' });
      return;
    }

    // Forward all query params except 'cluster' to OpenCost
    // Express may parse comma-separated values (e.g. window=start,end) as arrays
    const params = new URLSearchParams();
    for (const [key, value] of Object.entries(req.query)) {
      if (key === 'cluster') continue;
      if (typeof value === 'string') {
        params.set(key, value);
      } else if (Array.isArray(value)) {
        params.set(key, value.join(','));
      }
    }

    logger.debug(`Allocation request for cluster=${cluster}, params=${params.toString()}`);

    const result = await service.fetchAllocation(cluster, params.toString());
    res.status(result.status).setHeader('Content-Type', result.contentType).send(result.body);
  });

  /**
   * GET /costs/years?cluster=X
   * Returns distinct years that have cost data for the given cluster.
   */
  router.get('/costs/years', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    if (!cluster) {
      res.status(400).json({ message: 'Required: cluster' });
      return;
    }

    try {
      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [] });
        return;
      }

      const years = await costStore.getAvailableYears(clusterId);
      res.json({ data: years });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching available years for cluster=${cluster}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching available years' });
    }
  });

  /**
   * GET /costs/controllers?cluster=X&year=Y[&month=Z][&filter=preset][&q=text][&kinds=a,b][&excludeKinds=Job][&limit=50]
   * Server-side controller search for the filter dropdown. Results are ordered by
   * total cost and capped (default 50, max 500). `truncated` tells the client that
   * more rows matched, so it can ask the user to refine the query.
   * Without `month` the whole year is searched.
   */
  router.get('/costs/controllers', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const year = Number(req.query.year);
    const month = req.query.month === undefined ? undefined : Number(req.query.month);
    const list = (v: unknown) => (typeof v === 'string' && v ? v.split(',').filter(Boolean) : undefined);

    if (!cluster || !year || (month !== undefined && (!month || month < 1 || month > 12))) {
      res.status(400).json({ message: 'Required: cluster, year. Optional: month (1-12)' });
      return;
    }

    try {
      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [], truncated: false });
        return;
      }

      const { filter, error } = await parseFilter(req, cluster);
      if (error) {
        res.status(400).json({ message: error });
        return;
      }
      const result = await costStore.searchControllers(clusterId, year, month, filter, {
        q: typeof req.query.q === 'string' ? req.query.q : undefined,
        kinds: list(req.query.kinds),
        excludeKinds: list(req.query.excludeKinds),
        limit: req.query.limit === undefined ? undefined : Number(req.query.limit) || undefined,
      });
      res.json({ data: result.items, truncated: result.truncated });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching controllers for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching controllers' });
    }
  });

  /**
   * GET /costs/monthly-totals?cluster=X&year=Y&month=Z[&controllers=a,b][&filter=preset]
   * Returns one aggregated row for the month. Serves the yearly overview, which
   * previously pulled every per-pod row via /costs and summed in the browser.
   */
  router.get('/costs/monthly-totals', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const year = Number(req.query.year);
    const month = Number(req.query.month);
    if (!cluster || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, year, month (1-12)' });
      return;
    }
    try {
      const { filter, error: filterError } = await parseFilter(req, cluster);
      if (filterError) {
        res.status(400).json({ message: filterError });
        return;
      }

      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: null });
        return;
      }

      const data = await costStore.getMonthlyTotals(clusterId, year, month, filter);
      res.json({ data });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching monthly totals for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching monthly totals' });
    }
  });

  /**
   * GET /costs/daily-summary?cluster=X&year=Y&month=Z[&controllers=a,b][&filter=preset]
   * Returns per-day aggregated cost totals for a month from DB.
   */
  router.get('/costs/daily-summary', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const year = Number(req.query.year);
    const month = Number(req.query.month);
    if (!cluster || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, year, month (1-12)' });
      return;
    }
    try {
      const { filter, error: filterError } = await parseFilter(req, cluster);
      if (filterError) {
        res.status(400).json({ message: filterError });
        return;
      }

      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [] });
        return;
      }

      const data = await costStore.getDailySummary(clusterId, year, month, filter);
      res.json({ data });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching daily summary for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching daily summary' });
    }
  });

  /**
   * GET /costs/pods?cluster=X&date=YYYY-MM-DD[&controllers=a,b][&filter=preset]
   * Returns all pod costs for a specific date from DB.
   */
  router.get('/costs/pods', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const date = req.query.date as string | undefined;

    if (!cluster || !date) {
      res.status(400).json({ message: 'Required: cluster, date (YYYY-MM-DD)' });
      return;
    }
    try {
      const { filter, error: filterError } = await parseFilter(req, cluster);
      if (filterError) {
        res.status(400).json({ message: filterError });
        return;
      }

      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [] });
        return;
      }

      const data = await costStore.getPodsForDate(clusterId, date, filter);
      res.json({ data });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching pods for cluster=${cluster} date=${date}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching pod data' });
    }
  });

  /**
   * GET /costs?cluster=X&year=Y&month=Z[&controllers=a,b][&filter=preset]
   * Returns monthly pod cost data from DB.
   * Checks monthly_summaries first, falls back to real-time aggregation from daily_costs.
   */
  router.get('/costs', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const year = Number(req.query.year);
    const month = Number(req.query.month);
    if (!cluster || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, year, month (1-12)' });
      return;
    }
    try {
      const { filter, error: filterError } = await parseFilter(req, cluster);
      if (filterError) {
        res.status(400).json({ message: filterError });
        return;
      }

      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [], daysCovered: 0, source: 'none' });
        return;
      }

      // Try monthly summaries first
      const summaries = await costStore.getMonthlySummary(clusterId, year, month, filter);
      if (summaries.length > 0) {
        res.json({ data: summaries, daysCovered: summaries[0].daysCovered, source: 'monthly' });
        return;
      }

      // Fall back to real-time aggregation from daily costs
      const { rows, daysCovered } = await costStore.aggregateMonthOnTheFly(clusterId, year, month, filter);
      res.json({ data: rows, daysCovered, source: 'daily' });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching costs for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching cost data' });
    }
  });

  /**
   * GET /costs/daily?cluster=X&pod=POD&year=Y&month=Z
   * Returns daily cost breakdown for a specific pod in a given month.
   */
  router.get('/costs/daily', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const pod = req.query.pod as string | undefined;
    const year = Number(req.query.year);
    const month = Number(req.query.month);

    if (!cluster || !pod || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, pod, year, month (1-12)' });
      return;
    }

    try {
      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [] });
        return;
      }

      const startDate = `${year}-${String(month).padStart(2, '0')}-01`;
      const nextMonth = month === 12 ? 1 : month + 1;
      const nextYear = month === 12 ? year + 1 : year;
      const endDate = `${nextYear}-${String(nextMonth).padStart(2, '0')}-01`;

      const rows = await costStore.getDailyCostsForPod(clusterId, pod, startDate, endDate);
      res.json({ data: rows });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching daily costs for pod=${pod}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching daily cost data' });
    }
  });

  /**
   * GET /costs/collection-runs?cluster=X&year=Y&month=Z
   * Returns collection run info (start/finish times) per date for a month.
   */
  router.get('/costs/collection-runs', async (req, res) => {
    const cluster = req.query.cluster as string | undefined;
    const year = Number(req.query.year);
    const month = Number(req.query.month);

    if (!cluster || !year || !month || month < 1 || month > 12) {
      res.status(400).json({ message: 'Required: cluster, year, month (1-12)' });
      return;
    }

    try {
      const clusterId = await costStore.getClusterId(cluster);
      if (!clusterId) {
        res.json({ data: [] });
        return;
      }

      const startDate = `${year}-${String(month).padStart(2, '0')}-01`;
      const nextMonth = month === 12 ? 1 : month + 1;
      const nextYear = month === 12 ? year + 1 : year;
      const endDate = `${nextYear}-${String(nextMonth).padStart(2, '0')}-01`;

      const runs = await costStore.getCollectionRuns(clusterId, startDate, endDate);
      res.json({ data: runs });
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      logger.error(`Error fetching collection runs for cluster=${cluster} ${year}-${month}: ${msg}`);
      res.status(500).json({ message: 'Internal error fetching collection runs' });
    }
  });

  return router;
}
