import type {
  BooleanPropertySchema,
  CreateElicitationResponse,
  ElicitationSchema,
  ElicitationPropertySchema,
  EnumOption,
  IntegerPropertySchema,
  MultiSelectPropertySchema,
  NumberPropertySchema,
  StringPropertySchema,
} from "@agentclientprotocol/sdk";
import { CircleCheck, CircleX, ExternalLink, ListTodo, LoaderCircle, X } from "lucide-react";
import { useMemo, useState, type FormEvent } from "react";
import type { ExternalElicitationFlow, PendingElicitation } from "../../lib/state";
import { safeHttpUrl } from "../../lib/safe-url";
import { useInteractionFocus } from "../../lib/use-interaction-focus";
import { RawJson } from "./raw-json";

type FormValue = string | number | boolean | string[];

export function ElicitationCard({
  pending,
  onRespond,
}: {
  pending: PendingElicitation;
  onRespond: (response: CreateElicitationResponse) => void;
}) {
  const { request } = pending;
  const formRequest = request.mode === "form" && "requestedSchema" in request
    ? (request as typeof request & { requestedSchema: ElicitationSchema })
    : undefined;
  const urlRequest = request.mode === "url" && "url" in request
    ? (request as typeof request & { url: string })
    : undefined;
  const externalUrl = urlRequest ? safeHttpUrl(urlRequest.url) : undefined;
  const properties = formRequest?.requestedSchema.properties ?? {};
  const [values, setValues] = useState<Record<string, FormValue>>(() =>
    defaults(properties, new Set(formRequest?.requestedSchema.required ?? [])),
  );
  const [formError, setFormError] = useState<string>();
  const responding = pending.responseRequestId != null;
  const card = useInteractionFocus<HTMLFormElement>();

  const required = useMemo(
    () => new Set(formRequest?.requestedSchema.required ?? []),
    [formRequest],
  );
  const orderedProperties = useMemo(
    () => orderedPropertyEntries(
      properties,
      formRequest?.requestedSchema.required ?? [],
    ),
    [formRequest, properties],
  );

  const submit = (event: FormEvent) => {
    event.preventDefault();
    if (responding) return;
    const error = validateForm(properties, required, values);
    if (error) {
      setFormError(error);
      return;
    }
    setFormError(undefined);
    onRespond({ action: "accept", content: values });
  };

  return (
    <form
      ref={card}
      className="elicitation-card"
      role="dialog"
      aria-label="Agent input request"
      aria-busy={responding}
      onSubmit={submit}
      onKeyDown={(event) => {
        if (event.key !== "Escape" || responding) return;
        event.preventDefault();
        event.stopPropagation();
        onRespond({ action: "cancel" });
      }}
    >
      <div className="permission-title">
        <span className="permission-icon"><ListTodo size={17} /></span>
        <div><strong>Agent needs input</strong><p>{request.message}</p></div>
      </div>

      {formRequest ? (
        <fieldset className="elicitation-fields" disabled={responding}>
          {orderedProperties.map(([name, schema]) => (
            <ElicitationField
              key={name}
              name={name}
              schema={schema}
              required={required.has(name)}
              disabled={responding}
              value={values[name]}
              onChange={(value) => setValues((current) => {
                setFormError(undefined);
                if (value !== undefined) return { ...current, [name]: value };
                const next = { ...current };
                delete next[name];
                return next;
              })}
            />
          ))}
        </fieldset>
      ) : null}
      {formError ? <p className="elicitation-form-error" role="alert">{formError}</p> : null}

      {urlRequest ? (
        externalUrl ? (
          <a
            className="elicitation-url"
            href={externalUrl}
            target="_blank"
            rel="noreferrer"
            aria-disabled={responding}
            onClick={(event) => {
              if (responding) {
                event.preventDefault();
                return;
              }
              onRespond({ action: "accept" });
            }}
          >
            Open external flow <ExternalLink size={14} />
          </a>
        ) : (
          <p className="elicitation-url-error">Blocked non-HTTP elicitation URL.</p>
        )
      ) : null}

      <div className="permission-actions">
        {formRequest ? <button type="submit" disabled={responding}>Submit</button> : null}
        <button type="button" className="ghost" disabled={responding} onClick={() => onRespond({ action: "decline" })}>Decline</button>
        <button type="button" className="ghost" disabled={responding} onClick={() => onRespond({ action: "cancel" })}>Cancel</button>
      </div>
      {responding ? <p className="interaction-status" role="status">Sending response…</p> : null}
      {pending.responseError ? <p className="interaction-error" role="alert">{pending.responseError}</p> : null}
      <RawJson label="Input request" value={request} />
    </form>
  );
}

function orderedPropertyEntries(
  properties: Record<string, ElicitationPropertySchema>,
  requiredNames: string[],
): Array<[string, ElicitationPropertySchema]> {
  const entries = Object.entries(properties);
  const required = new Set(requiredNames);
  const byName = new Map(entries);
  return [
    ...requiredNames.flatMap((name) => {
      const schema = byName.get(name);
      return schema == null ? [] : [[name, schema] as [string, ElicitationPropertySchema]];
    }),
    ...entries.filter(([name]) => !required.has(name)),
  ];
}

export function ExternalFlowCard({
  flow,
  onDismiss,
}: {
  flow: ExternalElicitationFlow;
  onDismiss: () => void;
}) {
  const completed = flow.status === "completed";
  const cancelled = flow.status === "cancelled";
  const ended = completed || cancelled;
  return (
    <div className={`external-flow-card ${flow.status}`} role="status" aria-live="polite">
      {completed
        ? <CircleCheck size={16} />
        : cancelled
          ? <CircleX size={16} />
          : <LoaderCircle className="spin" size={16} />}
      <span>
        <strong>{completed
          ? "External flow completed"
          : cancelled
            ? "External flow cancelled"
            : "Waiting for external flow"}</strong>
        <small>{flow.message} · {flow.elicitationId}</small>
      </span>
      {flow.url && safeHttpUrl(flow.url) ? <a href={safeHttpUrl(flow.url)} target="_blank" rel="noreferrer">Open <ExternalLink size={11} /></a> : null}
      {ended ? <button type="button" aria-label="Dismiss external flow" onClick={onDismiss}><X size={13} /></button> : null}
    </div>
  );
}

function ElicitationField({
  name,
  schema,
  required,
  disabled,
  value,
  onChange,
}: {
  name: string;
  schema: ElicitationPropertySchema;
  required: boolean;
  disabled: boolean;
  value: FormValue | undefined;
  onChange: (value: FormValue | undefined) => void;
}) {
  const common = schema as { title?: string | null; description?: string | null };
  const title = common.title ?? name;
  const description = common.description;

  if (schema.type === "boolean") {
    const booleanSchema = schema as BooleanPropertySchema & { type: "boolean" };
    return (
      <label className="elicitation-toggle">
        <span><strong>{title}</strong>{description ? <small>{description}</small> : null}</span>
        <input disabled={disabled} type="checkbox" checked={typeof value === "boolean" ? value : Boolean(booleanSchema.default)} onChange={(event) => onChange(event.target.checked)} />
      </label>
    );
  }

  if (schema.type === "array") {
    const arraySchema = schema as MultiSelectPropertySchema & { type: "array" };
    const items = arraySchema.items as { anyOf?: EnumOption[]; enum?: string[] };
    const choices = items.anyOf && items.anyOf.length > 0
      ? items.anyOf.map((item) => ({ value: item.const, label: item.title }))
      : items.enum
        ? items.enum.map((item) => ({ value: item, label: item }))
        : [];
    const selected = Array.isArray(value) ? value : [];
    return (
      <fieldset disabled={disabled}>
        <legend>{title}{required ? " *" : ""}</legend>
        {description ? <small>{description}</small> : null}
        {choices.map((choice) => (
          <label className="check-choice" key={choice.value}>
            <input
              disabled={disabled}
              type="checkbox"
              checked={selected.includes(choice.value)}
              onChange={(event) => {
                const next = event.target.checked
                  ? [...selected, choice.value]
                  : selected.filter((item) => item !== choice.value);
                onChange(next.length > 0 || required ? next : undefined);
              }}
            />
            {choice.label}
          </label>
        ))}
      </fieldset>
    );
  }

  if (
    schema.type === "string" &&
    (((schema as StringPropertySchema).oneOf?.length ?? 0) > 0 ||
      ((schema as StringPropertySchema).enum?.length ?? 0) > 0)
  ) {
    const stringSchema = schema as StringPropertySchema & { type: "string" };
    const choices = stringSchema.oneOf && stringSchema.oneOf.length > 0
      ? stringSchema.oneOf.map((item) => ({ value: item.const, label: item.title }))
      : (stringSchema.enum ?? []).map((item) => ({ value: item, label: item }));
    return (
      <label>
        <strong>{title}{required ? " *" : ""}</strong>
        {description ? <small>{description}</small> : null}
        <select disabled={disabled} required={required} value={String(value ?? "")} onChange={(event) => onChange(event.target.value || undefined)}>
          {!required ? <option value="">Not specified</option> : null}
          {choices.map((choice) => <option value={choice.value} key={choice.value}>{choice.label}</option>)}
        </select>
      </label>
    );
  }

  if (schema.type === "number" || schema.type === "integer") {
    const numberSchema = schema as (NumberPropertySchema | IntegerPropertySchema) & { type: "number" | "integer" };
    return (
      <label>
        <strong>{title}{required ? " *" : ""}</strong>
        {description ? <small>{description}</small> : null}
        <input
          disabled={disabled}
          required={required}
          type="number"
          step={schema.type === "integer" ? 1 : "any"}
          min={numberSchema.minimum ?? undefined}
          max={numberSchema.maximum ?? undefined}
          value={typeof value === "number" ? value : ""}
          onChange={(event) => onChange(event.target.value === "" ? undefined : Number(event.target.value))}
        />
      </label>
    );
  }

  if (schema.type === "string") {
    const stringSchema = schema as StringPropertySchema & { type: "string" };
    return (
      <label>
        <strong>{title}{required ? " *" : ""}</strong>
        {description ? <small>{description}</small> : null}
        <input
          disabled={disabled}
          required={required}
          type={stringSchema.format === "email" ? "email" : stringSchema.format === "uri" ? "url" : stringSchema.format === "date" ? "date" : "text"}
          placeholder={stringSchema.format === "date-time" ? "2026-08-30T12:30:00Z" : undefined}
          minLength={stringSchema.minLength ?? undefined}
          maxLength={stringSchema.maxLength ?? undefined}
          pattern={stringSchema.pattern ?? undefined}
          value={typeof value === "string" ? value : ""}
          onChange={(event) => {
            const input = event.target.value;
            if (!input) {
              onChange(required ? "" : undefined);
            } else {
              onChange(input);
            }
          }}
        />
      </label>
    );
  }

  return <RawJson label={name} value={schema} />;
}

function defaults(
  properties: Record<string, ElicitationPropertySchema>,
  required: Set<string>,
): Record<string, FormValue> {
  const result: Record<string, FormValue> = {};
  for (const [name, schema] of Object.entries(properties)) {
    const candidate = (schema as { default?: unknown }).default;
    if (
      typeof candidate === "string" ||
      typeof candidate === "number" ||
      typeof candidate === "boolean" ||
      (Array.isArray(candidate) && candidate.every((item) => typeof item === "string"))
    ) {
      result[name] = candidate;
      continue;
    }
    if (!required.has(name)) continue;
    if (schema.type === "boolean") {
      result[name] = false;
    } else if (schema.type === "array") {
      result[name] = [];
    } else if (schema.type === "string") {
      const stringSchema = schema as StringPropertySchema;
      const first = stringSchema.oneOf?.[0]?.const ?? stringSchema.enum?.[0];
      if (first != null) result[name] = first;
    }
  }
  return result;
}

function validateForm(
  properties: Record<string, ElicitationPropertySchema>,
  required: Set<string>,
  values: Record<string, FormValue>,
): string | undefined {
  for (const name of required) {
    const schema = properties[name];
    if (!schema || !supportedField(schema)) return `Required field ${name} is not supported by this client.`;
    if (!(name in values)) return `Complete required field ${name}.`;
  }
  for (const [name, value] of Object.entries(values)) {
    const schema = properties[name];
    if (schema?.type !== "array" || !Array.isArray(value)) continue;
    const arraySchema = schema as MultiSelectPropertySchema;
    if (arraySchema.minItems != null && value.length < arraySchema.minItems) {
      return `Select at least ${arraySchema.minItems} option${arraySchema.minItems === 1 ? "" : "s"} for ${name}.`;
    }
    if (arraySchema.maxItems != null && value.length > arraySchema.maxItems) {
      return `Select no more than ${arraySchema.maxItems} options for ${name}.`;
    }
  }
  return undefined;
}

function supportedField(schema: ElicitationPropertySchema): boolean {
  if (["string", "number", "integer", "boolean"].includes(schema.type)) return true;
  if (schema.type !== "array") return false;
  const items = (schema as MultiSelectPropertySchema).items as {
    anyOf?: EnumOption[];
    enum?: string[];
  };
  return Boolean(items.anyOf?.length || items.enum?.length);
}
