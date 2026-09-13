import { apiBasePath, methodOrder } from "../config.mjs";
import {
  toCamelCase,
  toKebabCase,
  toPascalCase,
  singularize,
} from "../naming.mjs";
import { operationArgsType, paramsType } from "../schema-renderer.mjs";

function domainName(routePath, operation) {
  const tag = operation.tags?.[0];
  if (tag) return toKebabCase(tag);
  const segment = routePath
    .replace(apiBasePath, "")
    .split("/")
    .filter(Boolean)[0];
  return toKebabCase(segment ?? "root");
}

function operationName(method, routePath, usedNames) {
  const segments = routePath
    .replace(apiBasePath, "")
    .split("/")
    .filter(Boolean);
  const lastSegment = segments.at(-1) ?? "root";
  const hasPathParam = segments.some((segment) => segment.startsWith("{"));
  const prefix =
    method === "get" &&
    !lastSegment.startsWith("{") &&
    lastSegment.endsWith("s")
      ? "list"
      : method;
  const nameParts = segments
    .filter((segment) => !segment.startsWith("{"))
    .map((segment, index) => {
      const shouldSingularize =
        hasPathParam && index === 0 && segment.endsWith("s");
      return toPascalCase(shouldSingularize ? singularize(segment) : segment);
    });

  let name = `${prefix}${nameParts.join("") || "Root"}`;
  let suffix = 2;
  while (usedNames.has(name)) {
    name = `${prefix}${nameParts.join("") || "Root"}${suffix}`;
    suffix += 1;
  }
  usedNames.add(name);
  return name;
}

function collectParameters(pathItem, operation) {
  return [...(pathItem.parameters ?? []), ...(operation.parameters ?? [])];
}

function operationArgsRequired(pathParamsType) {
  return pathParamsType ? "" : "?";
}

export function collectQueryOptionOperations(spec) {
  const usedNames = new Set();
  const operations = [];

  for (const [routePath, pathItem] of Object.entries(spec.paths ?? {})) {
    for (const method of methodOrder) {
      const operation = pathItem?.[method];
      if (!operation || method !== "get") continue;

      const name = operation.operationId
        ? toCamelCase(operation.operationId)
        : operationName(method, routePath, usedNames);
      usedNames.add(name);
      const typeName = toPascalCase(name);
      const parameters = collectParameters(pathItem, operation);
      const pathParamsType = paramsType(parameters, "path");
      const queryParamsType = paramsType(parameters, "query");

      operations.push({
        name,
        typeName,
        domain: domainName(routePath, operation),
        method,
        routePath,
        argsType: operationArgsType(pathParamsType, queryParamsType, null),
        argsRequired: operationArgsRequired(pathParamsType),
        hasPathParams: Boolean(pathParamsType),
        hasQueryParams: Boolean(queryParamsType),
      });
    }
  }

  return operations;
}
