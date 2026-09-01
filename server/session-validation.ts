import type {
  SessionConfigOption,
  SessionConfigSelectOptions,
  SessionModeState,
} from "@agentclientprotocol/sdk";

const MAX_SESSION_CONTROLS = 256;
const MAX_SELECT_VALUES = 512;
const MAX_CONTROL_BYTES = 1_000_000;
const MAX_IDENTIFIER_LENGTH = 256;
const MAX_LABEL_LENGTH = 4_096;

export function validateSessionControls(
  modes: SessionModeState | null | undefined,
  configOptions: SessionConfigOption[] | null | undefined,
): void {
  if (modes != null) validateSessionModes(modes);
  validateSessionConfigOptions(configOptions ?? []);
  const bytes = Buffer.byteLength(JSON.stringify({ modes, configOptions }), "utf8");
  if (bytes > MAX_CONTROL_BYTES) {
    throw new Error(`Agent session controls exceed the ${MAX_CONTROL_BYTES} byte limit`);
  }
}

export function validateSessionModes(modes: SessionModeState): void {
  if (modes.availableModes.length === 0) {
    throw new Error("Agent session modes must contain at least one available mode");
  }
  if (modes.availableModes.length > MAX_SESSION_CONTROLS) {
    throw new Error(`Agent returned more than ${MAX_SESSION_CONTROLS} session modes`);
  }
  const ids = new Set<string>();
  for (const mode of modes.availableModes) {
    validateIdentifier(mode.id, "session mode ID");
    validateLabel(mode.name, "session mode name");
    if (ids.has(mode.id)) throw new Error(`Agent returned duplicate session mode ID: ${mode.id}`);
    ids.add(mode.id);
  }
  if (!ids.has(modes.currentModeId)) {
    throw new Error(`Agent current mode was not included in available modes: ${modes.currentModeId}`);
  }
}

export function validateSessionModeReference(
  modes: SessionModeState | null | undefined,
  modeId: string,
): void {
  if (!modes?.availableModes.some(({ id }) => id === modeId)) {
    throw new Error(`Mode was not offered by the Agent: ${modeId}`);
  }
}

export function validateSessionConfigOptions(options: SessionConfigOption[]): void {
  if (options.length > MAX_SESSION_CONTROLS) {
    throw new Error(`Agent returned more than ${MAX_SESSION_CONTROLS} config options`);
  }
  const ids = new Set<string>();
  for (const option of options) {
    validateIdentifier(option.id, "config option ID");
    validateLabel(option.name, "config option name");
    if (ids.has(option.id)) {
      throw new Error(`Agent returned duplicate config option ID: ${option.id}`);
    }
    ids.add(option.id);
    if (option.type === "select") validateSelectOption(option.id, option.currentValue, option.options);
  }
}

function validateSelectOption(
  configId: string,
  currentValue: string,
  options: SessionConfigSelectOptions,
): void {
  if (options.length === 0) {
    throw new Error(`Agent config option ${configId} has no selectable values`);
  }
  const values = new Set<string>();
  const groups = new Set<string>();
  for (const item of options) {
    if ("options" in item) {
      validateIdentifier(item.group, `config option ${configId} group ID`);
      validateLabel(item.name, `config option ${configId} group name`);
      if (groups.has(item.group)) {
        throw new Error(`Agent config option ${configId} has duplicate group ID: ${item.group}`);
      }
      groups.add(item.group);
      if (item.options.length === 0) {
        throw new Error(`Agent config option ${configId} contains an empty option group`);
      }
      for (const option of item.options) addSelectValue(configId, values, option.value, option.name);
    } else {
      addSelectValue(configId, values, item.value, item.name);
    }
    if (values.size > MAX_SELECT_VALUES) {
      throw new Error(
        `Agent config option ${configId} has more than ${MAX_SELECT_VALUES} selectable values`,
      );
    }
  }
  if (!values.has(currentValue)) {
    throw new Error(
      `Agent config option ${configId} current value was not included in its selectable values`,
    );
  }
}

function addSelectValue(
  configId: string,
  values: Set<string>,
  value: string,
  name: string,
): void {
  validateIdentifier(value, `config option ${configId} value`);
  validateLabel(name, `config option ${configId} value name`);
  if (values.has(value)) {
    throw new Error(`Agent config option ${configId} has duplicate value: ${value}`);
  }
  values.add(value);
}

function validateIdentifier(value: string, label: string): void {
  if (value.length === 0 || value.length > MAX_IDENTIFIER_LENGTH) {
    throw new Error(`${label} must contain between 1 and ${MAX_IDENTIFIER_LENGTH} characters`);
  }
}

function validateLabel(value: string, label: string): void {
  if (value.length === 0 || value.length > MAX_LABEL_LENGTH) {
    throw new Error(`${label} must contain between 1 and ${MAX_LABEL_LENGTH} characters`);
  }
}
