import { mkdir, writeFile } from "node:fs/promises";
import { dirname } from "node:path";
import { request, type FullConfig } from "@playwright/test";

import {
  ACCOUNTS,
  ADMIN_USERNAME,
  E2E_PASSWORD,
  storageStatePath,
  type E2EAccount,
} from "./accounts";

// Matches playwright.config.ts. Not 8080: that belongs to `make dev`.
const API = "http://127.0.0.1:8090";

type ApiContext = Awaited<ReturnType<typeof signIn>>;

// Creates the role-holding accounts once per run and leaves each one's
// signed-in cookies on disk, so no test spends time at the login form.
//
// The accounts are made through the API rather than the UI. Signing four users
// up by hand would add half a minute to every run and would test the create
// form four times over - which is Phase 2's job, once.
export default async function globalSetup(_config: FullConfig) {
  const admin = await signIn(ADMIN_USERNAME);

  for (const account of ACCOUNTS) {
    if (account.username !== ADMIN_USERNAME) await createAccount(admin, account);
    await saveSignedInState(account);
  }

  await admin.dispose();
}

async function signIn(username: string) {
  const context = await request.newContext({ baseURL: API });
  const response = await context.post("/api/v1/login", {
    data: { username, password: E2E_PASSWORD },
  });

  if (!response.ok()) {
    throw new Error(
      `e2e setup: could not sign in as ${username} (${response.status()}). ` +
        "If this is the admin, the database is not the one this suite expects: the " +
        "server only bootstraps an admin when no user exists, so run `make e2e`, " +
        "which deletes .e2e-data first.",
    );
  }

  return context;
}

// Idempotent, so `pnpm test:e2e` can be re-run against a database that already
// has these accounts. Only `make e2e` deletes the database; a setup that
// refused to run twice would make iterating on a single spec needlessly slow.
async function createAccount(admin: ApiContext, account: E2EAccount) {
  const roleIds: number[] = [];

  if (account.grants) {
    const name = `${account.username}-role`;
    // "*" because these accounts test what the UI reveals, not which
    // repositories a pattern matches.
    const created = await admin.post("/api/v1/roles", {
      data: {
        name,
        description: `e2e ${account.role}`,
        permissions: [{ repo_pattern: "*", actions: account.grants }],
      },
    });

    if (created.ok()) roleIds.push((await created.json()).id);
    else if (created.status() === 409) roleIds.push(await findRoleId(admin, name));
    else throw failed(`create role ${name}`, created.status());
  }

  const created = await admin.post("/api/v1/users", {
    data: {
      username: account.username,
      password: E2E_PASSWORD,
      role_ids: roleIds.length ? roleIds : undefined,
    },
  });

  // A conflict means a previous run left it, with the same password and role.
  if (!created.ok() && created.status() !== 409) {
    throw failed(`create user ${account.username}`, created.status());
  }
}

async function findRoleId(admin: ApiContext, name: string): Promise<number> {
  const response = await admin.get("/api/v1/roles");
  const roles = (await response.json()) as { id: number; name: string }[];
  const role = roles.find((candidate) => candidate.name === name);

  if (!role) throw new Error(`e2e setup: role ${name} conflicted but is not in the list`);

  return role.id;
}

// Signs in through a fresh context and writes its cookies out. Sessions are
// signed stateless cookies, so several workers may present the same one at once
// without invalidating each other - which is what lets the whole suite run in
// parallel across roles.
async function saveSignedInState(account: E2EAccount) {
  const context = await signIn(account.username);
  const path = storageStatePath(account.role);

  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, JSON.stringify(await context.storageState(), null, 2));
  await context.dispose();
}

function failed(what: string, status: number): Error {
  return new Error(`e2e setup: ${what} failed (${status})`);
}
