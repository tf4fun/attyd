import type {
  SessionConfigOption,
  SessionConfigSelectOption,
  SessionConfigSelectOptions,
} from "@agentclientprotocol/sdk";
import { Check, ChevronDown, Search, SlidersHorizontal } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";

type LegacyModes = {
  availableModes: Array<{ id: string; name: string }>;
  currentModeId: string;
};

export function SessionControls({
  options,
  modes,
  currentMode,
  disabled,
  onMode,
  onConfig,
}: {
  options: SessionConfigOption[];
  modes: LegacyModes | null | undefined;
  currentMode?: string;
  disabled: boolean;
  onMode: (id: string) => void;
  onConfig: (id: string, value: string | boolean) => void;
}) {
  // ACP configOptions supersede the legacy modes field. Rendering both produces
  // duplicate controls for agents that support the current and legacy shapes.
  const hasConfigMode = options.some(
    (option) => option.id === "mode" || option.category === "mode",
  );

  if ((!modes || hasConfigMode) && options.length === 0) return null;

  return (
    <div className="session-config-strip" aria-label="Session controls">
      <SlidersHorizontal className="config-strip-icon" size={14} aria-hidden="true" />
      {modes && !hasConfigMode ? (
        <SelectControl
          id="legacy-mode"
          name="Mode"
          value={currentMode ?? modes.currentModeId}
          options={modes.availableModes.map((mode) => ({
            value: mode.id,
            name: mode.name,
          }))}
          disabled={disabled}
          onChange={onMode}
        />
      ) : null}
      {options.map((option) => (
        <ConfigControl
          key={option.id}
          option={option}
          disabled={disabled}
          onChange={(value) => onConfig(option.id, value)}
        />
      ))}
    </div>
  );
}

function ConfigControl({
  option,
  disabled,
  onChange,
}: {
  option: SessionConfigOption;
  disabled: boolean;
  onChange: (value: string | boolean) => void;
}) {
  if (option.type === "boolean") {
    return (
      <button
        type="button"
        className="config-toggle"
        role="switch"
        aria-checked={option.currentValue}
        disabled={disabled}
        title={option.description ?? option.name}
        onClick={() => onChange(!option.currentValue)}
      >
        <span>{option.name}</span>
        <i aria-hidden="true"><span /></i>
      </button>
    );
  }

  return (
    <SelectControl
      id={option.id}
      name={option.name}
      description={option.description ?? undefined}
      value={option.currentValue}
      options={flattenOptions(option.options)}
      disabled={disabled}
      onChange={onChange}
    />
  );
}

function SelectControl({
  id,
  name,
  description,
  value,
  options,
  disabled,
  onChange,
}: {
  id: string;
  name: string;
  description?: string;
  value: string;
  options: SessionConfigSelectOption[];
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const current = options.find((option) => option.value === value);
  const searchable = options.length >= 8;
  const filtered = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    if (!normalized) return options;
    return options.filter((option) =>
      `${option.name} ${option.value} ${option.description ?? ""}`
        .toLocaleLowerCase()
        .includes(normalized)
    );
  }, [options, query]);

  useEffect(() => {
    if (!open) return;
    const closeOnPointer = (event: PointerEvent) => {
      if (!root.current?.contains(event.target as Node)) setOpen(false);
    };
    const closeOnEscape = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      event.preventDefault();
      setOpen(false);
      trigger.current?.focus();
    };
    document.addEventListener("pointerdown", closeOnPointer);
    window.addEventListener("keydown", closeOnEscape);
    if (searchable) requestAnimationFrame(() => search.current?.focus());
    return () => {
      document.removeEventListener("pointerdown", closeOnPointer);
      window.removeEventListener("keydown", closeOnEscape);
    };
  }, [open, searchable]);

  useEffect(() => {
    if (disabled) setOpen(false);
  }, [disabled]);

  const select = (nextValue: string) => {
    setOpen(false);
    setQuery("");
    if (nextValue !== value) onChange(nextValue);
    requestAnimationFrame(() => trigger.current?.focus());
  };

  return (
    <div className={`config-picker ${open ? "open" : ""}`} ref={root}>
      <button
        ref={trigger}
        type="button"
        className="config-trigger"
        aria-haspopup="listbox"
        aria-expanded={open}
        aria-controls={`config-options-${id}`}
        disabled={disabled}
        title={description ?? `${name}: ${current?.name ?? value}`}
        onClick={() => {
          setQuery("");
          setOpen((value) => !value);
        }}
      >
        <span>{name}</span>
        <strong>{current?.name ?? value}</strong>
        <ChevronDown size={12} aria-hidden="true" />
      </button>
      {open ? (
        <div className="config-popover">
          <div className="config-popover-heading">
            <strong>{name}</strong>
            {description ? <small>{description}</small> : null}
          </div>
          {searchable ? (
            <label className="config-search">
              <Search size={13} aria-hidden="true" />
              <input
                ref={search}
                aria-label={`Search ${name}`}
                value={query}
                placeholder={`Search ${options.length} options`}
                onChange={(event) => setQuery(event.target.value)}
              />
            </label>
          ) : null}
          <div className="config-option-list" id={`config-options-${id}`} role="listbox" aria-label={name}>
            {filtered.map((option) => (
              <button
                type="button"
                role="option"
                aria-selected={option.value === value}
                key={option.value}
                onClick={() => select(option.value)}
              >
                <span>
                  <strong>{option.name}</strong>
                  {option.description ? <small>{option.description}</small> : null}
                </span>
                {option.value === value ? <Check size={14} aria-hidden="true" /> : null}
              </button>
            ))}
            {filtered.length === 0 ? <p>No matching options</p> : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function flattenOptions(options: SessionConfigSelectOptions): SessionConfigSelectOption[] {
  return options.flatMap((option) => "options" in option ? option.options : option);
}
