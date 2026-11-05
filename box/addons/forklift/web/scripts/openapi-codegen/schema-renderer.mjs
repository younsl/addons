import { toPascalCase } from "./naming.mjs";

function refName(ref) {
  return ref.split("/").at(-1);
}

function normalizeSchemaType(type) {
  if (Array.isArray(type)) {
    const nonNull = type.filter((value) => value !== "null");
    return {
      type: nonNull[0],
      nullable: type.includes("null"),
    };
  }
  return { type, nullable: false };
}

function quotePropertyName(name) {
  return /^[A-Za-z_$][A-Za-z0-9_$]*$/.test(name) ? name : JSON.stringify(name);
}

function enumLiteral(value) {
  return typeof value === "number" || typeof value === "boolean"
    ? String(value)
    : JSON.stringify(value);
}

export function schemaType(schema, contextName = "Inline") {
  if (!schema) return "unknown";
  if (schema.$ref) return refName(schema.$ref);
  if (schema.allOf) {
    return schema.allOf
      .map((item, index) => schemaType(item, `${contextName}${index + 1}`))
      .join(" & ");
  }
  if (schema.oneOf || schema.anyOf) {
    return (schema.oneOf ?? schema.anyOf)
      .map((item, index) => schemaType(item, `${contextName}${index + 1}`))
      .join(" | ");
  }
  if (schema.enum) return schema.enum.map(enumLiteral).join(" | ");

  const { type, nullable } = normalizeSchemaType(schema.type);
  let rendered;
  switch (type) {
    case "integer":
    case "number":
      rendered = "number";
      break;
    case "boolean":
      rendered = "boolean";
      break;
    case "array":
      rendered = arrayType(schema.items, `${contextName}Item`);
      break;
    case "object":
      rendered = objectType(schema, contextName);
      break;
    case "string":
      rendered = "string";
      break;
    // A bare "null" branch inside a oneOf/anyOf, which is how the document
    // makes a $ref nullable. Left as "unknown" it would swallow the union.
    case "null":
      rendered = "null";
      break;
    default:
      rendered = "unknown";
  }
  return nullable ? `${rendered} | null` : rendered;
}

function arrayType(items, contextName) {
  const itemType = schemaType(items, contextName);
  const needsParentheses = itemType.includes(" | ") || itemType.includes(" & ");
  return `${needsParentheses ? `(${itemType})` : itemType}[]`;
}

function objectType(schema, contextName) {
  if (schema.properties) {
    const required = new Set(schema.required ?? []);
    const fields = Object.entries(schema.properties).map(([name, property]) => {
      const optional = required.has(name) ? "" : "?";
      return `${quotePropertyName(name)}${optional}: ${schemaType(
        property,
        `${contextName}${toPascalCase(name)}`,
      )};`;
    });
    return `{ ${fields.join(" ")} }`;
  }
  if (schema.additionalProperties) {
    return `Record<string, ${schemaType(
      schema.additionalProperties,
      `${contextName}Value`,
    )}>`;
  }
  return "Record<string, unknown>";
}

export function renderComponentType(name, schema) {
  const rendered = schemaType(schema, name);
  if (rendered.startsWith("{ ") && rendered.endsWith(" }")) {
    return `export interface ${name} ${rendered}\n`;
  }
  return `export type ${name} = ${rendered};\n`;
}

export function paramsType(parameters, location) {
  const params = parameters.filter((parameter) => parameter.in === location);
  if (params.length === 0) return null;
  const fields = params.map((parameter) => {
    const optional = parameter.required ? "" : "?";
    return `${quotePropertyName(parameter.name)}${optional}: ${schemaType(
      parameter.schema,
      `${toPascalCase(location)}${toPascalCase(parameter.name)}`,
    )}`;
  });
  return `{ ${fields.join("; ")} }`;
}

export function operationArgsType(pathParamsType, queryParamsType, bodyType) {
  const fields = [];
  if (pathParamsType) fields.push(`path: ${pathParamsType}`);
  if (queryParamsType) fields.push(`query?: ${queryParamsType}`);
  if (bodyType) fields.push(`body: ${bodyType}`);
  return fields.length > 0 ? `{ ${fields.join("; ")} }` : null;
}

export function requestBodyType(operation, operationTypeName) {
  const schema = operation.requestBody?.content?.["application/json"]?.schema;
  return schema ? schemaType(schema, `${operationTypeName}RequestBody`) : null;
}

export function responseType(operation, operationTypeName) {
  const response =
    operation.responses?.["200"] ??
    operation.responses?.["201"] ??
    operation.responses?.["204"];
  if (!response || !response.content) return "void";
  const schema = response.content["application/json"]?.schema;
  return schema ? schemaType(schema, `${operationTypeName}Response`) : "void";
}
