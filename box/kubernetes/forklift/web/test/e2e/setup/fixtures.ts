import { test as base, expect, request, type APIRequestContext, type Page } from "@playwright/test";

import { ADMIN_USERNAME, E2E_PASSWORD, storageStatePath, type E2ERole } from "./accounts";

// Matches playwright.config.ts. Not 8080: that belongs to `make dev`.
const API = "http://127.0.0.1:8090";

type Fixtures = {
  // Opens a page already signed in as the given role.
  signedInAs: (role: E2ERole) => Promise<Page>;
  // An administrator's API context, for arranging what a test needs before it
  // starts driving the browser - and for reading state the signed-in role
  // cannot. A user with no role sees an empty repository list, so a test that
  // needs a repository id has to ask as somebody who can see one.
  adminApi: APIRequestContext;
};

export const test = base.extend<Fixtures>({
  signedInAs: async ({ browser }, use, testInfo) => {
    const contexts: Awaited<ReturnType<typeof browser.newContext>>[] = [];
    const failures: string[] = [];

    await use(async (role) => {
      const context = await browser.newContext({ storageState: storageStatePath(role) });
      contexts.push(context);

      const page = await context.newPage();
      // The assertions read English labels; without this they follow whatever
      // the machine's locale happens to be.
      // Merged rather than written outright, and that is not fussiness: every
      // preference now shares one storage key, so replacing it would wipe the
      // others on every load - including a reload, which is exactly what the
      // "collapsing survives a reload" test is checking.
      await page.addInitScript(() => {
        const key = "forklift.user-preferences.v1";
        const stored = window.localStorage.getItem(key);
        const parsed = stored ? JSON.parse(stored) : { state: {}, version: 1 };

        window.localStorage.setItem(
          key,
          JSON.stringify({ ...parsed, state: { ...parsed.state, language: "en" } }),
        );
      });

      page.on("console", (message) => {
        if (message.type() === "error" && !isExpectedNetworkNoise(message.text())) {
          failures.push(`console: ${message.text()} (${message.location().url})`);
        }
      });
      page.on("pageerror", (error) => failures.push(`uncaught: ${error.message}`));

      return page;
    });

    for (const context of contexts) await context.close();

    // Reported after the test body so a real assertion failure is seen first -
    // a console error is usually its cause, not a separate problem.
    //
    // This check is the reason the fixture exists. A React render that throws
    // still paints something, so an assertion on visible text can pass over a
    // broken screen.
    if (failures.length > 0 && testInfo.status === testInfo.expectedStatus) {
      throw new Error(`page reported errors:\n  ${dedupe(failures).join("\n  ")}`);
    }
  },

  adminApi: async ({}, use) => {
    const context = await request.newContext({ baseURL: API });
    const response = await context.post("/api/v1/login", {
      data: { username: ADMIN_USERNAME, password: E2E_PASSWORD },
    });

    if (!response.ok()) {
      throw new Error(`e2e: admin sign-in failed (${response.status()}); run \`make e2e\``);
    }

    await use(context);
    await context.dispose();
  },
});

export { expect };

// Names an entity so parallel workers cannot collide, and so a leftover from a
// failed run is recognisable.
//
// Never assert on a total count in a suite that runs in parallel: another
// worker creating a row breaks it. Assert on your own name, or on a delta.
export function scopedName(prefix: string): string {
  const { parallelIndex, title } = test.info();
  const slug = title.toLowerCase().replace(/[^a-z0-9]+/g, "-").slice(0, 20);

  return `e2e-${prefix}-${parallelIndex}-${slug}`.replace(/-+$/, "");
}

// One repository the server seeds on an empty database, used read-only. Seeded
// repositories are protected from deletion, which makes them safe to share
// across parallel workers.
export const SEEDED_REPOSITORY = "maven-hosted";

export async function seededRepositoryId(adminApi: APIRequestContext): Promise<number> {
  const response = await adminApi.get("/api/v1/repositories");
  const repositories = (await response.json()) as { id: number; name: string }[];
  const seeded = repositories.find((repository) => repository.name === SEEDED_REPOSITORY);

  if (!seeded) {
    throw new Error(
      `seed repository ${SEEDED_REPOSITORY} is missing. The server creates it on an ` +
        "empty database, so a leftover .e2e-data can leave it out - run `make e2e`.",
    );
  }

  return seeded.id;
}

// A pending approval is the one piece of state the API cannot arrange. POST
// /approvals only records a decision that has already been made - approved or
// rejected - and the screens that matter here (the bulk approver, the sidebar's
// badge) exist only while something is *pending*. The sole producer of a pending
// row is the approval gate itself, so a test that needs one has to be refused by
// it.
//
// The repository is HOSTED, and that is not an arbitrary choice. The approval
// gate is the last thing before the bytes go out, which on a proxy means it
// runs *after* the upstream fetch has already succeeded - so a proxy pointed at
// an address nothing answers never reaches the gate at all; it fails upstream
// and returns 502. A hosted repository has no upstream to go wrong: the file is
// already stored, so the gate is reached on the first read.
//
// Raw is the format because a raw path is its own package name: no coordinate
// parsing stands between the request and the queue.
export async function createApprovalGatedRepository(
  adminApi: APIRequestContext,
  name: string,
): Promise<number> {
  const response = await adminApi.post("/api/v1/repositories", {
    data: {
      name,
      format: "raw",
      type: "hosted",
      upstream_url: "",
      config: {
        cache: { enabled: true, metadata_ttl: "15m", negative_ttl: "5m", eviction: "lru" },
        age_policy: { enabled: false },
        approval: { enabled: true, mode: "enforce" },
        policy_pipeline: { schema_version: 2, order: ["vulnerability", "license", "age"] },
      },
    },
  });

  if (!response.ok()) {
    throw new Error(`e2e: could not create the gated repository (${response.status()})`);
  }

  return (await response.json()).id;
}

// Leaves one pending approval behind. The file has to be published first -
// there is nothing to gate until there are bytes - and the read that follows is
// what the gate refuses, recording the request as pending.
//
// Deleting the repository afterwards takes the approval with it, which is how
// these tests avoid disturbing the shared queue every other worker is reading.
export async function requestBlockedPackage(
  adminApi: APIRequestContext,
  repository: string,
  packageName: string,
) {
  const published = await adminApi.put(`/raw/${repository}/${packageName}`, {
    data: "e2e payload",
    headers: { "content-type": "application/octet-stream" },
  });

  if (!published.ok()) {
    throw new Error(`e2e: could not publish the file to gate (${published.status()})`);
  }

  const response = await adminApi.get(`/raw/${repository}/${packageName}`);

  if (response.status() !== 403) {
    throw new Error(
      `e2e: expected the approval gate to refuse the read, got ${response.status()}. ` +
        "Without a refusal nothing is queued, and the screens under test have no row.",
    );
  }
}

// The repository-pattern field is a Base UI combobox, not a plain input, and
// typing into it is not enough: the value is only committed on Enter or by
// picking an option. Until then the DOM input shows the text while the
// component's own value is still empty, so "Add permission" stays disabled and
// the failure reads as a missing button rather than an uncommitted field.
//
// Worth knowing beyond the tests: a user who types a pattern that is not an
// existing repository name - which is the case this control exists for - has to
// press Enter before the Add button will do anything.
//
// Shared because three screens use the same control: role permissions, token
// scopes, and the token scope modal.
export async function typeRepositoryPattern(page: Page, pattern: string) {
  const input = page.getByPlaceholder(/repo pattern/i);

  await input.click();
  await input.pressSequentially(pattern);
  await input.press("Enter");
}

// The browser itself writes a console error for every failed request, and a
// refused request is not a defect here: the shell probes /me before it knows
// whether anyone is signed in, and a role without admin rights is *supposed* to
// be turned away from the listings the sidebar asks for. The app handles both -
// it suppresses the toast and renders the screen - so failing the test on them
// would mean no screen could ever be visited by a limited role.
//
// Narrow otherwise: a 500, a CORS failure, or anything the application itself
// logged still fails the test.
function isExpectedNetworkNoise(text: string): boolean {
  return /Failed to load resource.*\b(401|403)\b/.test(text);
}

// A render loop reports the same message hundreds of times; one copy is enough
// to identify it and the rest only buries the real failure.
function dedupe(messages: string[]): string[] {
  const seen = new Map<string, number>();
  for (const message of messages) seen.set(message, (seen.get(message) ?? 0) + 1);

  return [...seen].map(([message, count]) => (count > 1 ? `${message}  (×${count})` : message));
}
