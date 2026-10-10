import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptDir = path.dirname(fileURLToPath(import.meta.url));
const webRoot = path.resolve(scriptDir, "../..");

export const specPath = path.resolve(
  webRoot,
  "../src/openapi/openapi.yaml",
);
export const outDir = path.resolve(webRoot, "src/services/v1");
export const legacyGeneratedV1Dir = path.resolve(webRoot, "src/generated/v1");
export const legacyQueryGeneratedDir = path.resolve(
  webRoot,
  "src/query/generated",
);
export const legacyServicesGeneratedDir = path.resolve(
  webRoot,
  "src/services/generated",
);
export const queryGeneratedV1Dir = path.resolve(webRoot, "src/query/v1");
export const queryOptionsOutPath = path.resolve(
  queryGeneratedV1Dir,
  "openapi-query-options.ts",
);
export const queryKeysOutPath = path.resolve(
  queryGeneratedV1Dir,
  "openapi-query-keys.ts",
);
export const commonTypesOutPath = path.resolve(outDir, "openapi-types.ts");
export const runtimeOutPath = path.resolve(outDir, "openapi-runtime.ts");

export const methodOrder = ["get", "post", "put", "patch", "delete"];
export const apiBasePath = "/api/v1";
