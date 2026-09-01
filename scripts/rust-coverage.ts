import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

interface RustCoverageSummary {
  data: Array<{ totals: { lines: { percent: number } } }>;
}

const workspace = process.cwd();
const minimumLines = Number(process.env.ATTYD_MIN_RUST_COVERAGE ?? "85");
if (!Number.isFinite(minimumLines) || minimumLines < 0 || minimumLines > 100) {
  throw new Error("ATTYD_MIN_RUST_COVERAGE must be a percentage from 0 through 100");
}

const temporaryDirectory = await mkdtemp(join(tmpdir(), "attyd-rust-coverage-"));
const reportPath = join(temporaryDirectory, "rust.json");
const coverageTarget = join(workspace, "target", "llvm-cov-target");

try {
  run("cargo", ["llvm-cov", "clean", "--workspace", "--offline"]);
  run("cargo", ["llvm-cov", "--offline", "--all-targets", "--no-report"], {
    ATTYD_SKIP_WEB_BUILD: "1",
  });

  const instrumentedEnvironment = rustCoverageEnvironment(coverageTarget);
  run("cargo", ["build", "--offline"], {
    ...instrumentedEnvironment,
    ATTYD_SKIP_WEB_BUILD: "1",
  });
  const rustBinary = join(
    coverageTarget,
    "debug",
    process.platform === "win32" ? "attyd.exe" : "attyd",
  );
  run("node", ["--import", "tsx", "scripts/ui-smoke.ts"], {
    ...instrumentedEnvironment,
    ATTYD_RUST_BINARY: rustBinary,
    ATTYD_SMOKE_SKIP_OVERSIZED_LINE: "1",
  });
  run("node", ["--import", "tsx", "scripts/rust-remote-smoke.ts"], {
    ...instrumentedEnvironment,
    ATTYD_RUST_BINARY: rustBinary,
  });

  run("cargo", [
    "llvm-cov",
    "report",
    "--offline",
    "--json",
    "--summary-only",
    "--output-path",
    reportPath,
  ]);
  run("cargo", ["llvm-cov", "report", "--offline"]);
  const summary = JSON.parse(await readFile(reportPath, "utf8")) as RustCoverageSummary;
  const lines = summary.data[0]?.totals.lines.percent;
  if (lines == null || !Number.isFinite(lines)) {
    throw new Error("Rust coverage report did not contain a finite line percentage");
  }

  console.log(`\nRust backend line coverage: ${lines.toFixed(2)}%`);
  if (lines + Number.EPSILON < minimumLines) {
    throw new Error(
      `Rust backend line coverage ${lines.toFixed(2)}% is below ${minimumLines.toFixed(2)}%`,
    );
  }
  console.log(`Result: PASS (minimum ${minimumLines.toFixed(2)}%)`);
} finally {
  await rm(temporaryDirectory, { recursive: true, force: true });
}

function rustCoverageEnvironment(coverageTargetDirectory: string): NodeJS.ProcessEnv {
  const result = spawnSync("cargo", ["llvm-cov", "show-env", "--sh"], {
    cwd: workspace,
    env: { ...process.env, CARGO_TARGET_DIR: coverageTargetDirectory },
    encoding: "utf8",
  });
  if (result.status !== 0) {
    throw new Error(
      `cargo llvm-cov show-env failed (${String(result.status)}): ${result.stderr}`,
    );
  }

  const environment: NodeJS.ProcessEnv = {
    ...process.env,
    CARGO_TARGET_DIR: coverageTargetDirectory,
  };
  for (const line of result.stdout.split(/\r?\n/u)) {
    const match = line.match(/^export ([A-Za-z_][A-Za-z0-9_]*)=(.*)$/u);
    if (!match) continue;
    environment[match[1]] = decodeShellValue(match[2]);
  }
  return environment;
}

function decodeShellValue(value: string): string {
  if (value.startsWith("'") && value.endsWith("'")) {
    return value.slice(1, -1).replaceAll("'\\''", "'");
  }
  return value;
}

function run(
  command: string,
  args: string[],
  environment: NodeJS.ProcessEnv = process.env,
): void {
  const result = spawnSync(command, args, {
    cwd: workspace,
    env: { ...process.env, ...environment },
    stdio: "inherit",
  });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    throw new Error(`${command} ${args.join(" ")} failed with status ${String(result.status)}`);
  }
}
