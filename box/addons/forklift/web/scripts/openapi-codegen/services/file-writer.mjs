import { mkdir, rm, writeFile } from "node:fs/promises";
import path from "node:path";

import {
  commonTypesOutPath,
  legacyServicesGeneratedDir,
  outDir,
  runtimeOutPath,
} from "../config.mjs";
import {
  renderCommonTypesFile,
  renderDomainServicesFile,
  renderDomainTypesFile,
  renderRuntimeFile,
} from "./file-renderer.mjs";

export async function writeGeneratedFiles(spec, operationsByDomain) {
  const schemaNames = new Set(Object.keys(spec.components?.schemas ?? {}));

  await rm(legacyServicesGeneratedDir, { recursive: true, force: true });
  await rm(outDir, { recursive: true, force: true });
  await mkdir(outDir, { recursive: true });
  await writeFile(commonTypesOutPath, renderCommonTypesFile(spec));
  await writeFile(runtimeOutPath, renderRuntimeFile(spec));

  for (const [domain, operations] of operationsByDomain) {
    const domainDir = path.resolve(outDir, domain);
    await mkdir(domainDir, { recursive: true });
    await writeFile(
      path.resolve(domainDir, "types.ts"),
      renderDomainTypesFile(spec, domain, operations, schemaNames),
    );
    await writeFile(
      path.resolve(domainDir, "api.ts"),
      renderDomainServicesFile(spec, domain, operations),
    );
  }
}
