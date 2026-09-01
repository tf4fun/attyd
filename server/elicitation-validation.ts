import type {
  CreateElicitationRequest,
  CreateElicitationResponse,
  ElicitationSchema,
  ElicitationPropertySchema,
  IntegerPropertySchema,
  MultiSelectPropertySchema,
  NumberPropertySchema,
  StringPropertySchema,
} from "@agentclientprotocol/sdk";

const MAX_ELICITATION_FIELDS = 64;
const MAX_ELICITATION_CHOICES = 256;
const MAX_PATTERN_LENGTH = 512;
const MAX_ELICITATION_BYTES = 2_000_000;
const MAX_ELICITATION_MESSAGE_LENGTH = 16_384;
const MAX_ELICITATION_FIELD_NAME_LENGTH = 256;
const MAX_ELICITATION_RESPONSE_BYTES = 2_000_000;

export function validateElicitationRequest(request: CreateElicitationRequest): void {
  if (Buffer.byteLength(JSON.stringify(request), "utf8") > MAX_ELICITATION_BYTES) {
    throw new Error(`Elicitation request exceeds ${MAX_ELICITATION_BYTES} bytes`);
  }
  if (
    request.message.length === 0 ||
    request.message.length > MAX_ELICITATION_MESSAGE_LENGTH
  ) {
    throw new Error("Elicitation message is empty or too long");
  }
  validateElicitationScope(request);
  if (request.mode !== "form" && request.mode !== "url") {
    throw new Error(`Agent requested an unadvertised elicitation mode: ${request.mode}`);
  }
  if (request.mode === "url") {
    if (!("url" in request) || typeof request.url !== "string") {
      throw new Error("URL elicitation is missing its URL");
    }
    let url: URL;
    try {
      url = new URL(request.url);
    } catch {
      throw new Error("URL elicitation contains an invalid URL");
    }
    if (url.protocol !== "http:" && url.protocol !== "https:") {
      throw new Error("URL elicitation must use HTTP or HTTPS");
    }
    return;
  }
  if (!("requestedSchema" in request)) {
    throw new Error("Form elicitation is missing requestedSchema");
  }
  const schema = request.requestedSchema as ElicitationSchema;
  const properties = schema.properties ?? {};
  const entries = Object.entries(properties);
  if (entries.length > MAX_ELICITATION_FIELDS) {
    throw new Error(`Elicitation schema has more than ${MAX_ELICITATION_FIELDS} fields`);
  }
  const required = schema.required ?? [];
  if (new Set(required).size !== required.length) {
    throw new Error("Elicitation schema contains duplicate required fields");
  }
  for (const name of required) {
    if (!Object.hasOwn(properties, name)) {
      throw new Error(`Elicitation schema requires an unknown field: ${name}`);
    }
  }
  for (const [name, property] of entries) {
    if (name.length === 0 || name.length > MAX_ELICITATION_FIELD_NAME_LENGTH) {
      throw new Error("Elicitation schema contains an invalid field name");
    }
    validatePropertySchema(name, property);
  }
}

function validateElicitationScope(request: CreateElicitationRequest): void {
  const hasSessionId = "sessionId" in request && request.sessionId != null;
  const hasRequestId = "requestId" in request;
  if (hasSessionId === hasRequestId) {
    throw new Error("Elicitation must have exactly one session or request scope");
  }
  if (hasSessionId) {
    if (typeof request.sessionId !== "string" || request.sessionId.length === 0 || request.sessionId.length > 1_024) {
      throw new Error("Elicitation contains an invalid session scope");
    }
    return;
  }
  if (!("requestId" in request)) {
    throw new Error("Elicitation is missing its request scope");
  }
  if (request.requestId === null) return;
  if (
    !(
      (typeof request.requestId === "string" && request.requestId.length <= 1_024) ||
      (typeof request.requestId === "number" && Number.isSafeInteger(request.requestId))
    )
  ) {
    throw new Error("Elicitation contains an invalid request scope");
  }
}

export function validateElicitationResponse(
  request: CreateElicitationRequest,
  response: CreateElicitationResponse,
): void {
  if (Buffer.byteLength(JSON.stringify(response), "utf8") > MAX_ELICITATION_RESPONSE_BYTES) {
    throw new Error(`Elicitation response exceeds ${MAX_ELICITATION_RESPONSE_BYTES} bytes`);
  }
  if (!["accept", "decline", "cancel"].includes(response.action)) {
    throw new Error(`Unsupported elicitation response action: ${response.action}`);
  }
  if (response.action !== "accept") {
    if ("content" in response && response.content != null) {
      throw new Error("Only accepted elicitation responses may contain content");
    }
    return;
  }

  if (request.mode === "url") {
    if (response.content != null) {
      throw new Error("URL elicitation responses must not contain content");
    }
    return;
  }

  if (!("requestedSchema" in request)) {
    throw new Error("Form elicitation is missing requestedSchema");
  }
  const requestedSchema = request.requestedSchema as ElicitationSchema;
  const content = response.content ?? {};
  if (!isRecord(content)) {
    throw new Error("Form elicitation content must be an object");
  }
  const properties = requestedSchema.properties ?? {};
  for (const name of requestedSchema.required ?? []) {
    if (!Object.hasOwn(content, name)) {
      throw new Error(`Elicitation response is missing required field: ${name}`);
    }
  }
  for (const [name, value] of Object.entries(content)) {
    const schema = properties[name];
    if (!schema) throw new Error(`Unknown elicitation response field: ${name}`);
    validateField(name, value, schema);
  }
}

function validateField(
  name: string,
  value: unknown,
  schema: ElicitationPropertySchema,
): void {
  switch (schema.type) {
    case "string": {
      const stringSchema = schema as StringPropertySchema;
      if (typeof value !== "string") invalidType(name, "string");
      if (stringSchema.minLength != null && value.length < stringSchema.minLength) {
        throw new Error(`Elicitation field ${name} is shorter than minLength`);
      }
      if (stringSchema.maxLength != null && value.length > stringSchema.maxLength) {
        throw new Error(`Elicitation field ${name} is longer than maxLength`);
      }
      if (stringSchema.pattern != null) {
        if (value.length > 10_000) {
          throw new Error(`Elicitation field ${name} is too long for pattern validation`);
        }
        let pattern: RegExp;
        try {
          pattern = new RegExp(stringSchema.pattern, "u");
        } catch {
          throw new Error(`Elicitation field ${name} has an invalid schema pattern`);
        }
        if (!pattern.test(value)) {
          throw new Error(`Elicitation field ${name} does not match pattern`);
        }
      }
      if (stringSchema.format != null && !matchesFormat(value, stringSchema.format)) {
        throw new Error(`Elicitation field ${name} is not a valid ${stringSchema.format}`);
      }
      const allowed = nonEmpty(stringSchema.oneOf)?.map(({ const: item }) => item)
        ?? nonEmpty(stringSchema.enum);
      if (allowed && !allowed.includes(value)) {
        throw new Error(`Elicitation field ${name} is not an allowed value`);
      }
      return;
    }
    case "number":
    case "integer": {
      const numberSchema = schema as NumberPropertySchema | IntegerPropertySchema;
      if (
        typeof value !== "number" ||
        !Number.isFinite(value) ||
        (schema.type === "integer" && !Number.isInteger(value))
      ) {
        invalidType(name, schema.type);
      }
      if (numberSchema.minimum != null && value < numberSchema.minimum) {
        throw new Error(`Elicitation field ${name} is below minimum`);
      }
      if (numberSchema.maximum != null && value > numberSchema.maximum) {
        throw new Error(`Elicitation field ${name} is above maximum`);
      }
      return;
    }
    case "boolean":
      if (typeof value !== "boolean") invalidType(name, "boolean");
      return;
    case "array": {
      const arraySchema = schema as MultiSelectPropertySchema;
      if (!Array.isArray(value) || !value.every((item) => typeof item === "string")) {
        invalidType(name, "string array");
      }
      if (new Set(value).size !== value.length) {
        throw new Error(`Elicitation field ${name} contains duplicate selections`);
      }
      if (arraySchema.minItems != null && value.length < arraySchema.minItems) {
        throw new Error(`Elicitation field ${name} has too few selections`);
      }
      if (arraySchema.maxItems != null && value.length > arraySchema.maxItems) {
        throw new Error(`Elicitation field ${name} has too many selections`);
      }
      const items = arraySchema.items as {
        anyOf?: Array<{ const: string }>;
        enum?: string[];
      };
      const allowed = nonEmpty(items.anyOf)?.map(({ const: item }) => item)
        ?? nonEmpty(items.enum);
      if (allowed && value.some((item) => !allowed.includes(item))) {
        throw new Error(`Elicitation field ${name} contains an unknown selection`);
      }
      return;
    }
    default:
      throw new Error(`Unsupported elicitation field type for ${name}: ${schema.type}`);
  }
}

function validatePropertySchema(
  name: string,
  schema: ElicitationPropertySchema,
): void {
  switch (schema.type) {
    case "string": {
      const stringSchema = schema as StringPropertySchema;
      validateLengthBounds(name, stringSchema.minLength, stringSchema.maxLength);
      if (stringSchema.pattern != null) assertSafePattern(name, stringSchema.pattern);
      const enumValues = nonEmpty(stringSchema.enum);
      const oneOf = nonEmpty(stringSchema.oneOf);
      if (enumValues && oneOf) {
        throw new Error(`Elicitation field ${name} defines both enum and oneOf`);
      }
      validateChoices(name, enumValues ?? oneOf?.map(({ const: value }) => value));
      validateDefaultValue(name, schema);
      return;
    }
    case "number":
    case "integer": {
      const numberSchema = schema as NumberPropertySchema | IntegerPropertySchema;
      if (numberSchema.minimum != null && !Number.isFinite(numberSchema.minimum)) {
        throw new Error(`Elicitation field ${name} has a non-finite minimum`);
      }
      if (numberSchema.maximum != null && !Number.isFinite(numberSchema.maximum)) {
        throw new Error(`Elicitation field ${name} has a non-finite maximum`);
      }
      if (
        numberSchema.minimum != null &&
        numberSchema.maximum != null &&
        numberSchema.minimum > numberSchema.maximum
      ) {
        throw new Error(`Elicitation field ${name} has inverted numeric bounds`);
      }
      validateDefaultValue(name, schema);
      return;
    }
    case "array": {
      const arraySchema = schema as MultiSelectPropertySchema;
      validateLengthBounds(name, arraySchema.minItems, arraySchema.maxItems);
      const items = arraySchema.items as {
        anyOf?: Array<{ const: string }>;
        enum?: string[];
      };
      const enumValues = nonEmpty(items.enum);
      const anyOf = nonEmpty(items.anyOf);
      if (enumValues && anyOf) {
        throw new Error(`Elicitation field ${name} defines both enum and anyOf`);
      }
      validateChoices(name, enumValues ?? anyOf?.map(({ const: value }) => value));
      validateDefaultValue(name, schema);
      return;
    }
    case "boolean":
      validateDefaultValue(name, schema);
      return;
    default:
      return;
  }
}

function validateDefaultValue(
  name: string,
  schema: ElicitationPropertySchema,
): void {
  const candidate = (schema as { default?: unknown }).default;
  if (candidate != null) validateField(name, candidate, schema);
}

function validateLengthBounds(
  name: string,
  minimum: number | null | undefined,
  maximum: number | null | undefined,
): void {
  for (const [label, value] of [["minimum", minimum], ["maximum", maximum]] as const) {
    if (value != null && (!Number.isSafeInteger(value) || value < 0)) {
      throw new Error(`Elicitation field ${name} has an invalid ${label} length`);
    }
  }
  if (minimum != null && maximum != null && minimum > maximum) {
    throw new Error(`Elicitation field ${name} has inverted length bounds`);
  }
}

function validateChoices(name: string, values: string[] | undefined): void {
  if (!values) return;
  if (values.length > MAX_ELICITATION_CHOICES) {
    throw new Error(`Elicitation field ${name} has too many choices`);
  }
  if (new Set(values).size !== values.length) {
    throw new Error(`Elicitation field ${name} has duplicate choices`);
  }
}

function assertSafePattern(name: string, source: string): void {
  if (source.length > MAX_PATTERN_LENGTH) {
    throw new Error(`Elicitation field ${name} pattern is too long`);
  }
  try {
    new RegExp(source, "u");
  } catch {
    throw new Error(`Elicitation field ${name} has an invalid schema pattern`);
  }
  if (/\\[1-9]/u.test(source) || hasUnsafeQuantifiedGroup(source)) {
    throw new Error(`Elicitation field ${name} has an unsafe schema pattern`);
  }
}

function hasUnsafeQuantifiedGroup(source: string): boolean {
  const stack: Array<{ complex: boolean }> = [{ complex: false }];
  let inClass = false;
  let escaped = false;
  let closedGroup: { complex: boolean } | undefined;
  for (let index = 0; index < source.length; index += 1) {
    const character = source[index];
    if (escaped) {
      escaped = false;
      closedGroup = undefined;
      continue;
    }
    if (character === "\\") {
      escaped = true;
      closedGroup = undefined;
      continue;
    }
    if (character === "[" && !inClass) {
      inClass = true;
      closedGroup = undefined;
      continue;
    }
    if (character === "]" && inClass) {
      inClass = false;
      continue;
    }
    if (inClass) continue;
    if (character === "(" && source[index + 1] === "?" && source[index + 2] !== ":") {
      return true;
    }
    if (character === "(") {
      stack.push({ complex: false });
      closedGroup = undefined;
      continue;
    }
    if (character === ")" && stack.length > 1) {
      const group = stack.pop()!;
      if (group.complex) stack.at(-1)!.complex = true;
      closedGroup = group;
      continue;
    }
    const quantifier = character === "*" || character === "+" || character === "{";
    if (quantifier) {
      if (closedGroup?.complex) return true;
      stack.at(-1)!.complex = true;
      closedGroup = undefined;
      continue;
    }
    if (character === "|") stack.at(-1)!.complex = true;
    if (!/\s/u.test(character)) closedGroup = undefined;
  }
  return false;
}

function invalidType(name: string, expected: string): never {
  throw new Error(`Elicitation field ${name} must be ${expected}`);
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function nonEmpty<T>(value: T[] | null | undefined): T[] | undefined {
  return value && value.length > 0 ? value : undefined;
}

function matchesFormat(
  value: string,
  format: NonNullable<StringPropertySchema["format"]>,
): boolean {
  switch (format) {
    case "email":
      return /^[^\s@]+@[^\s@]+\.[^\s@]+$/u.test(value);
    case "uri":
      try {
        return Boolean(new URL(value).protocol);
      } catch {
        return false;
      }
    case "date":
      return validDate(value);
    case "date-time": {
      const match = /^(\d{4}-\d{2}-\d{2})T\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:\d{2})$/u.exec(value);
      return match != null && validDate(match[1]) && !Number.isNaN(Date.parse(value));
    }
  }
}

function validDate(value: string): boolean {
  const match = /^(\d{4})-(\d{2})-(\d{2})$/u.exec(value);
  if (!match) return false;
  const year = Number(match[1]);
  const month = Number(match[2]);
  const day = Number(match[3]);
  const date = new Date(Date.UTC(year, month - 1, day));
  return date.getUTCFullYear() === year &&
    date.getUTCMonth() === month - 1 &&
    date.getUTCDate() === day;
}
