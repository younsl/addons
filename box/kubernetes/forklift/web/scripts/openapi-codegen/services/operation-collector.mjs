import { apiBasePath, methodOrder } from "../config.mjs";
import {
  toCamelCase,
  toKebabCase,
  toPascalCase,
  singularize,
} from "../naming.mjs";
import {
  operationArgsType,
  paramsType,
  requestBodyType,
  responseType,
} from "../schema-renderer.mjs";

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
  const resourceSegments = segments.filter(
    (segment) => !segment.startsWith("{"),
  );
  const lastResourceSegment = resourceSegments.at(-1) ?? "root";

  let prefix = method;
  if (
    method === "get" &&
    !lastSegment.startsWith("{") &&
    lastSegment.endsWith("s")
  ) {
    prefix = "list";
  } else if (method === "post" && lastSegment.endsWith("s")) {
    prefix = "create";
  } else if (method === "put" || method === "patch") {
    prefix = "update";
  }

  const nameParts = resourceSegments.map((segment, index) => {
    const shouldSingularize =
      hasPathParam && index === 0 && segment.endsWith("s");
    return toPascalCase(shouldSingularize ? singularize(segment) : segment);
  });

  if (
    method === "post" &&
    !lastSegment.endsWith("s") &&
    lastResourceSegment === lastSegment
  ) {
    const action = toPascalCase(lastSegment);
    const resource = nameParts.slice(0, -1).join("");
    nameParts.splice(0, nameParts.length, action, resource);
  }

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

export function collectOperationsByDomain(spec) {
  const usedNames = new Set();
  const operationsByDomain = new Map();

  for (const [routePath, pathItem] of Object.entries(spec.paths ?? {})) {
    for (const method of methodOrder) {
      const operation = pathItem?.[method];
      if (!operation) continue;

      const name = operation.operationId
        ? toCamelCase(operation.operationId)
        : operationName(method, routePath, usedNames);
      usedNames.add(name);
      const typeName = toPascalCase(name);
      const parameters = collectParameters(pathItem, operation);
      const pathParamsType = paramsType(parameters, "path");
      const queryParamsType = paramsType(parameters, "query");
      const bodyType = requestBodyType(operation, typeName);
      const domain = domainName(routePath, operation);
      const domainOperations = operationsByDomain.get(domain) ?? [];

      domainOperations.push({
        name,
        typeName,
        method,
        routePath,
        domain,
        argsType: operationArgsType(pathParamsType, queryParamsType, bodyType),
        hasPathParams: Boolean(pathParamsType),
        hasQueryParams: Boolean(queryParamsType),
        hasBody: Boolean(bodyType),
        responseType: responseType(operation, typeName),
      });
      operationsByDomain.set(domain, domainOperations);
    }
  }

  return operationsByDomain;
}
