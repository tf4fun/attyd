import type {
  SessionConfigOption,
  SessionConfigSelectOption,
  SessionConfigSelectGroup,
  SessionConfigSelectOptions,
} from "@agentclientprotocol/sdk";
import { Check, ChevronDown, Search, SlidersHorizontal } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "../../i18n";

type LegacyModes = {
  availableModes: Array<{ id: string; name: string; description?: string | null }>;
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
  options: SessionConfigOption[] | null;
  modes: LegacyModes | null | undefined;
  currentMode?: string;
  disabled: boolean;
  onMode: (id: string) => void;
  onConfig: (id: string, value: string | boolean) => void;
}) {
  const { t } = useTranslation("workspace");
  const hasConfigOptions = options != null;
  if (hasConfigOptions ? options.length === 0 : !modes) return null;

  return (
    <div className="session-config-strip" aria-label={t("controls.label")}>
      <SlidersHorizontal className="config-strip-icon" size={14} aria-hidden="true" />
      {modes && !hasConfigOptions ? (
        <SelectControl
          id="legacy-mode"
          name={t("controls.mode")}
          value={currentMode ?? modes.currentModeId}
          options={modes.availableModes.map((mode) => ({
            value: mode.id,
            name: mode.name,
            description: mode.description,
          }))}
          disabled={disabled}
          onChange={onMode}
        />
      ) : null}
      {options?.map((option) => (
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
      options={option.options}
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
  options: SessionConfigSelectOptions;
  disabled: boolean;
  onChange: (value: string) => void;
}) {
  const { t } = useTranslation("workspace");
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const popover = useRef<HTMLDivElement>(null);
  const search = useRef<HTMLInputElement>(null);
  const groups = useMemo<Array<{ group?: string; name?: string; options: SessionConfigSelectOption[] }>>(
    () => isGroupedOptions(options) ? options : [{ options }], [options],
  );
  const values = groups.flatMap((group) => group.options);
  const currentGroup = groups.find((group) => group.options.some((option) => option.value === value));
  const current = currentGroup?.options.find((option) => option.value === value);
  const currentLabel = [currentGroup?.name, current?.name ?? value].filter(Boolean).join(" · ");
  const searchable = values.length >= 8;
  const filtered = useMemo(() => {
    const normalized = query.trim().toLocaleLowerCase();
    if (!normalized) return groups;
    return groups.flatMap((group) => {
      const options = group.options.filter((option) =>
        `${group.name ?? ""} ${option.name} ${option.value} ${option.description ?? ""}`
          .toLocaleLowerCase().includes(normalized)
      );
      return options.length > 0 ? [{ ...group, options }] : [];
    });
  }, [groups, query]);

  useLayoutEffect(() => {
    if (!open) return;
    const position = () => {
      if (!root.current || !popover.current) return;
      const left = root.current.getBoundingClientRect().left;
      const width = popover.current.getBoundingClientRect().width;
      const offset = Math.max(8 - left, Math.min(0, document.documentElement.clientWidth - 8 - left - width));
      popover.current.style.setProperty("--config-popover-offset", `${offset}px`);
    };
    position();
    window.addEventListener("resize", position);
    return () => window.removeEventListener("resize", position);
  }, [open]);

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
        title={[description, t("controls.currentValue", { name, value: currentLabel })].filter(Boolean).join("\n")}
        onClick={() => {
          setQuery("");
          setOpen((value) => !value);
        }}
      >
        <span>{name}</span>
        <strong>{currentLabel}</strong>
        <ChevronDown size={12} aria-hidden="true" />
      </button>
      {open ? (
        <div className="config-popover" ref={popover}>
          <div className="config-popover-heading">
            <strong>{name}</strong>
            {description ? <small>{description}</small> : null}
          </div>
          {searchable ? (
            <label className="config-search">
              <Search size={13} aria-hidden="true" />
              <input
                ref={search}
                aria-label={t("controls.search", { name })}
                value={query}
                placeholder={t("controls.searchOptions", { count: values.length })}
                onChange={(event) => setQuery(event.target.value)}
              />
            </label>
          ) : null}
          <div className="config-option-list" id={`config-options-${id}`} role="listbox" aria-label={name}>
            {filtered.map((group) => (
              <div key={group.group ?? "ungrouped"} role={group.name ? "group" : undefined} aria-label={group.name}>
                {group.name ? <div className="config-group-heading">{group.name}</div> : null}
                {group.options.map((option) => (
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
              </div>
            ))}
            {filtered.length === 0 ? <p>{t("controls.noMatchingOptions")}</p> : null}
          </div>
        </div>
      ) : null}
    </div>
  );
}

function isGroupedOptions(options: SessionConfigSelectOptions): options is SessionConfigSelectGroup[] {
  return options.length > 0 && "options" in options[0];
}
