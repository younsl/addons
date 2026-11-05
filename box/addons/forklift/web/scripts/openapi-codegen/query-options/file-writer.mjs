import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";

import {
  queryKeysOutPath,
  legacyGeneratedV1Dir,
  legacyQueryGeneratedDir,
  queryGeneratedV1Dir,
  queryOptionsOutPath,
} from "../config.mjs";
import {
  renderDomainQueryOptionsFile,
  renderQueryKeysFile,
  renderQueryOptionsFile,
} from "./file-renderer.mjs";

export async function writeQueryOptionsFile(spec, operations) {
  await rm(path.resolve(legacyGeneratedV1Dir, "query-options"), {
    recursive: true,
    force: true,
  });
  await rm(path.resolve(legacyGeneratedV1Dir, "openapi-query-keys.ts"), {
    force: true,
  });
  await rm(path.resolve(legacyGeneratedV1Dir, "openapi-query-options.ts"), {
    force: true,
  });
  await rm(legacyQueryGeneratedDir, { recursive: true, force: true });
  await rm(queryGeneratedV1Dir, { recursive: true, force: true });
  await mkdir(queryGeneratedV1Dir, { recursive: true });
  await writeFile(queryKeysOutPath, renderQueryKeysFile(spec, operations));

  for (const [domain, domainOperations] of operationsByDomain(operations)) {
    const domainDir = path.resolve(queryGeneratedV1Dir, domain);
    await mkdir(domainDir, { recursive: true });
    await writeFile(
      path.resolve(domainDir, "options.ts"),
      renderDomainQueryOptionsFile(spec, domain, domainOperations),
    );
  }

  await writeFile(
    queryOptionsOutPath,
    renderQueryOptionsFile(spec, operations),
  );
}

function operationsByDomain(operations) {
  const grouped = new Map();

  operations.forEach((operation) => {
    const domainOperations = grouped.get(operation.domain) ?? [];
    domainOperations.push(operation);
    grouped.set(operation.domain, domainOperations);
  });

  return Array.from(grouped.entries()).sort(([left], [right]) =>
    left.localeCompare(right),
  );
}
