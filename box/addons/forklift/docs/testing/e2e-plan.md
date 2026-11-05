# Plan: strengthening the browser tests

## Where this started

One test, nine lines. It checked that the login screen rendered.

```ts
test("renders the login screen", ...)   // the whole of test/e2e/auth.spec.ts
```

- `webServer` started **Vite only**. With no backend, signing in was impossible
- there were no accounts
- **it did not run in CI** (`ci-forklift.yml` does not mention playwright)

So there were effectively no browser tests. The work of opening 23 screens by
hand, one at a time, was work that should have been automated.

---

## 1. The harness

### 1-1. Playwright starts both servers

```ts
// playwright.config.ts
webServer: [
  {
    // The backend, started from the repository root.
    command: "cd .. && cargo run --locked --bin forklift",
    url: "http://127.0.0.1:8090/readyz",   // open without authentication
    env: {
      FORKLIFT_DATA_DIR: "./.e2e-data",
      FORKLIFT_HTTP_ADDR: "127.0.0.1:8090",
      FORKLIFT_METRICS_ADDR: "127.0.0.1:0",
      FORKLIFT_PPROF_ADDR: "127.0.0.1:0",
      FORKLIFT_BOOTSTRAP_ADMIN_USER: "e2e-admin",
      FORKLIFT_BOOTSTRAP_ADMIN_PASSWORD: "e2e-only-not-a-secret",
      // Without a fixed secret the server generates a throwaway one, and every
      // saved storageState is invalidated the moment it restarts.
      FORKLIFT_SESSION_SECRET: "e2e-only-not-a-secret",
      FORKLIFT_LOG_LEVEL: "warn",
    },
    reuseExistingServer: !process.env.CI,
    timeout: 600_000,
  },
  {
    command: "pnpm dev --host 127.0.0.1 --port 5273 --strictPort",
    url: "http://127.0.0.1:5273/login",
    env: { FORKLIFT_API_TARGET: "http://127.0.0.1:8090" },
    reuseExistingServer: !process.env.CI,
  },
],
```

**The password is set rather than scraped from the log.** `BootstrapAdmin`
treats `generated := password == ""`, so a supplied value is used as-is. Parsing
the log breaks whenever its format changes, and keeping this password secret is
not the point of a test.

### 1-2. The data directory is kept apart from `.data`

`make clean` deletes `.data`, which is the instance a developer is using. The
browser tests must not take it with them, so they use `.e2e-data`.

`BootstrapAdmin` is skipped when `CountUsers() > 0`. **The accounts are only
created against an empty database, so it has to be deleted before every run.**

```makefile
## e2e: run the browser tests from a clean state
e2e:
	rm -rf .e2e-data
	cd web && mise exec -- pnpm test:e2e
```

`.e2e-data` is added to `.gitignore` only. **`make clean` does not touch it** —
deleting it mid-run would be unwelcome, and the responsibility for deleting it
belongs to `make e2e`, immediately before it runs.

### 1-3. Global setup creates an account per role

The UI diverges sharply by permission. Per `utils/permissions.ts`, in five
directions:

| Account | Role | What it sees |
|---|---|---|
| `e2e-admin` | bootstrap administrator | everything |
| `e2e-auditor` | audit | the administrative screens, read-only |
| `e2e-approver` | approve | the approval queue, and may decide |
| `e2e-security` | security | may edit repository security policy |
| `e2e-plain` | none | the repository list and its own tokens |

Global setup signs in as the administrator, creates the roles and accounts
**through the API**, then signs in as each and leaves a `storageState` file
behind. The tests reuse that state, so signing in is paid for once.

```ts
// test/e2e/-setup/global-setup.ts
for (const account of ACCOUNTS) {
  await api.createRole(account.role);
  await api.createUser(account);
  await saveStorageState(account);          // test/e2e/.auth/<name>.json
}
```

`.auth/` is gitignored.

### 1-4. Isolation: by namespace

**Roles do not have to run serially.** The session is a signed, stateless
cookie (`codec.Encode`), so there is no server-side session table. Several
workers may hold the same cookie at once without invalidating one another, and
each has its own browser context.

The one place parallelism does break is the single backend database they share.
Rather than resetting it between tests, **each test names what it owns**:

```ts
const name = `e2e-${test.info().parallelIndex}-${slug(test.info().title)}`;
```

- the 15 seeded repositories are used as a **read-only fixture** (deleting them
  is refused with a 403 anyway)
- a destructive test creates its own repository and deletes it afterwards

#### What breaks most often under parallelism: absolute counts

Asserting "there are 15 repositories" breaks the moment a neighbouring worker
creates one. This is a rule, not a preference:

```ts
// ✗ breaks when another worker creates one
await expect(page.getByRole("row")).toHaveCount(15);

// ✓ look only at your own
await expect(page.getByRole("row", { name })).toBeVisible();

// ✓ or look at the change
const before = await countRows();
await createOne();
await expect.poll(countRows).toBe(before + 1);
```

#### What cannot run in parallel

| | Why |
|---|---|
| HA step-down | the leader changes. Genuinely global state |
| Purging a repository | safe as long as it is your own repository |

**Impersonation does run in parallel.** It issues a new cookie to one browser
context only, so no other worker's session is affected. It is kept in its own
file because the session changes underneath it, not because it must be serial.

---

## 2. The division of labour with the unit tests

There are already 152 unit tests. A browser test that repeats one is only slow.

| | Unit (vitest) | Browser (playwright) |
|---|---|---|
| Invalidation rules | which queries are asked to refetch | **whether the number on screen actually changes** |
| Error classification | `ApiError` → view model | whether the message reaches the screen |
| Pure functions | all of them | none |
| Permissions | the helper functions | **whether a route turns a role away** |
| Forms | the arguments a hook submits | a round trip to the server, reflected in the list |

The four invalidation bugs fixed alongside this plan sit exactly in the browser
column. A unit test proves an invalidation fired; **only a browser proves the
badge on the repository list went from 3 to 2.**

---

## 3. Coverage — 23 screens

### Phase 1. Smoke (every route × two roles)

The cheapest tests, and the ones that catch the most. This is the work that was
being done by hand.

For each route:

- a role that may see it finds **a landmark unique to that screen** (the `<h1>`,
  usually)
- a role that may not is **redirected**
- **no console errors** (collected via `page.on("console")`, asserted at the end)

| Route | Allowed | Where a refusal lands |
|---|---|---|
| `/login` | signed out | signed in → repositories |
| `/workspace/repositories` | everyone | — |
| `/workspace/repositories/new` | admin | → repositories |
| `/workspace/repositories/$id/$tab` × 7 | per tab | → artifacts |
| `/workspace/repositories/$id/upload` | upload permission | → artifacts |
| `/workspace/approvals`, `/$id`, `/bulk` | admin, approver, auditor | → repositories |
| `/workspace/tokens`, `/new` | everyone | — |
| `/access/users`, `/$id` | admin, auditor | → repositories |
| `/access/users/new`, `/$id/tokens/new` | admin | → repositories |
| `/access/roles`, `/$id` | admin, auditor | → repositories |
| `/access/roles/new` | admin | → repositories |
| `/admin/notifications`, `/new`, `/$id` | admin | → repositories |
| `/admin/storage`, `/admin/ha` | admin | → repositories |
| `/settings` | everyone | — |

Roughly 23 routes × 2 = **46 assertions**. The table is data, driven by
`test.each`.

### Phase 2. A round trip per domain

Created → appears in the list → edited → deleted. One file per domain.

| Domain | Scenario |
|---|---|
| roles | create (with permissions) → list → add and remove a permission → delete |
| users | create (user and robot) → assign and unassign a role → reset the password → disable → delete |
| tokens | create (choosing an expiry) → **the secret is shown once** → rescope → revoke |
| repositories | create (hosted, proxy, group) → save settings → take offline → delete |
| approvals | add a rule (allow, block, block a version) → decide from the queue → the status moves |
| notifications | create a receiver → **the webhook URL does not come back** → edit → delete |

The two in bold regress easily: a token secret cannot be seen again, and the
webhook URL is supposed to open blank, which reads like a bug.

### Phase 3. Freshness across screens ← the four bugs

```
a decision        → the pending badge on the repository list goes down
saving security   → the linked-repositories list on the receiver changes
a role permission → the role table on the repository Permissions tab changes
issuing a token   → the token table on the same tab gains a row
purging           → the artifact count and size on the list go to zero
```

Each one: write on screen A → navigate to screen B → the value has changed.

### Phase 4. The long tail

- the six upload formats (maven, npm, pypi, cargo, go, raw) — fixtures are
  built in the test rather than committed
- the policy pipeline's drag ordering, and the ReactFlow graph
- bulk approval (toggling Clean only)
- starting and ending impersonation — its own file, since the session changes
- HA step-down — a real failover, its own file
- global search (⌘K), collapsing the sidebar, switching language, content width

### Phase 5. CI — not added

The browser tests are not in CI. They run locally, from `make e2e`.

That means **the suite does not defend itself.** No commit hook and no pipeline
enforces it, so it depends on whoever touched the screens knowing to run it. Two
of the bugs it found (a render loop, and a dead `pattern` validation) were of a
kind that no amount of clicking through the screens would surface — which is the
argument for at least running it on any pull request that moves the UI much, and
recording the result in the description.

---

## Decisions

| | Decision | Why |
|---|---|---|
| Browsers | **chromium only** | this is an internal console; run time matters more than cross-browser risk |
| `make clean` | **leaves `.e2e-data` alone** | deleting it mid-run would be unwelcome. `make e2e` deletes it immediately before running instead |
| Upload fixtures | **built in the test** | no binaries in the repository. Some knowledge of each format leaks into the tests as a result, but the server validates that part anyway, so a minimal file is enough |
| CI | **not added** | run from `make e2e`, at the cost of a suite that is not self-enforcing |

---

## Suggested order

Phase 0 (harness) → Phase 1 (smoke) → Phase 3 (freshness) → Phase 2 (round
trips) → Phase 4.

Phase 3 comes before Phase 2 because it is where the bugs actually were, and it
is shorter than the round trips.

---

## Appendix: UI problems the tests exposed

Found by driving the screens rather than reading them. Most look like
inconveniences for the tests; 4 and 5 are accessibility defects.

| | Problem | Consequence |
|---|---|---|
| 1 | **Only the repository table has no `row-*` testid** | every other table (tokens, users, roles, receivers) has one. Here a test has to fall back to `getByRole("row").filter({ hasText })` |
| 2 | The sidebar's "Settings" is indistinguishable from the repository tab's "Settings" | worked around by filtering the `nav` on whether it contains "Artifacts" — which rests on the coincidence that the sidebar does not use that word. A `tab-<name>` testid is the answer |
| 3 | The quota's "Current usage" has no `value-*` | read by position (`nth(3)`). If the column order changes, the test silently reads the wrong number |
| 4 | **No field on the repository Settings tab has an `htmlFor`** | `getByLabel` does not work at all, so the tests find fields by placeholder. **A screen reader cannot associate the labels with their inputs** |
| 5 | **The token scope modal has no `role="dialog"` or `aria-modal`** | there is nothing to scope to, so controls inside the modal are located at page level. Contrast the delete confirmation, which is an `AlertDialog` and is reached cleanly with `getByRole("alertdialog")`. **Assistive technology does not recognise the modal as one** |
| 6 | Online/offline state has no stable text handle | the switch label and the descriptive paragraph both contain "offline", and Playwright matches text partially and case-insensitively, so the locator is ambiguous |

1–3 and 6 are fixed by adding testids. **4 and 5 have to be fixed in the
application, and are worth fixing regardless of the tests.**

### How the per-user token limit affects parallelism

`MAX_TOKENS_PER_USER` is 3, and the server enforces it with a 409. Tests that
create tokens therefore **spread themselves across accounts** — two workers
creating as the same user fail at creation rather than at an assertion, and the
failure message points nowhere near the cause.

## Connection-pool cancellation regression

The Rust store owns one write connection and a read pool. A cancelled async
caller must not discard the connection used by its blocking SQL operation.
The blocking task owns a connection lease and returns it when the operation
finishes, including during panic unwinding. Change detection keeps its pinned
connection across cancelled queries as well.

`src/meta/readpool_test.rs` covers cancellation, panic recovery and pinned
connection reuse. E2E runs must use fresh data and report every failure rather
than accepting failed requests as successful writes.
