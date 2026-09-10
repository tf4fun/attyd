export function devPort(value: string | undefined): number {
  if (value == null) return 5173;
  if (!/^\d+$/u.test(value) || Number(value) < 1 || Number(value) > 65_535) {
    throw new Error(`ATTYD_DEV_PORT must be an integer from 1 to 65535; received ${JSON.stringify(value)}`);
  }
  return Number(value);
}

// Stop at the Agent command, which may itself accept a --dev-server argument.
export function hasDevServer(args: string[]): boolean {
  const valueOptions = new Set([
    "--host", "-H", "--port", "-p", "--cwd", "-c", "--transport", "-t",
    "--allowed-origin", "--add-dir", "--mcp-config", "--session-unobserved-timeout",
  ]);
  for (let index = 0; index < args.length; index += 1) {
    const argument = args[index];
    if (argument === "--" || !argument.startsWith("-")) return false;
    if (argument === "--dev-server" || argument.startsWith("--dev-server=")) return true;
    if (valueOptions.has(argument)) index += 1;
  }
  return false;
}
