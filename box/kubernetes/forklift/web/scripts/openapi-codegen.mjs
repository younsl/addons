import { readFile } from "node:fs/promises";
import YAML from "yaml";

import { specPath } from "./openapi-codegen/config.mjs";
import { collectOperationsByDomain } from "./openapi-codegen/services/operation-collector.mjs";
import { writeGeneratedFiles } from "./openapi-codegen/services/file-writer.mjs";

async function readSpec() {
  const source = await readFile(specPath, "utf8");
  return YAML.parse(source);
}

async function main() {
  const spec = await readSpec();
  const operationsByDomain = collectOperationsByDomain(spec);
  await writeGeneratedFiles(spec, operationsByDomain);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
