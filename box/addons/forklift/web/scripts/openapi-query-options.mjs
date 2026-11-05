import { readFile } from "node:fs/promises";
import YAML from "yaml";

import { specPath } from "./openapi-codegen/config.mjs";
import { collectQueryOptionOperations } from "./openapi-codegen/query-options/operation-collector.mjs";
import { writeQueryOptionsFile } from "./openapi-codegen/query-options/file-writer.mjs";

async function readSpec() {
  const source = await readFile(specPath, "utf8");
  return YAML.parse(source);
}

async function main() {
  const spec = await readSpec();
  const operations = collectQueryOptionOperations(spec);
  await writeQueryOptionsFile(spec, operations);
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
