import { spawnSync } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

interface NodeCoverageSummary {
  total: { lines: { pct: number } };
}

interface RustCoverageSummary {
  data: Array<{ totals: { lines: { percent: number } } }>;
}

const workspace = process.cwd();
const temporaryDirectory = await mkdtemp(join(tmpdir(), "attyd-backend-coverage-"));
const nodeReportDirectory = join(temporaryDirectory, "node");
const rustReportPath = join(temporaryDirectory, "rust.json");
const coverageTarget = join(workspace, "target", "llvm-cov-target");

try {
  run("npx", [
    "vitest",
    "run",
    "--coverage",
    "--coverage.reporter=json-summary",
    `--coverage.reportsDirectory=${nodeReportDirectory}`,
    "--coverage.include=server/**/*.ts",
    "--coverage.include=shared/**/*.ts",
  ]);
  const nodeSummary = JSON.parse(
    await readFile(join(nodeReportDirectory, "coverage-summary.json"), "utf8"),
  ) as NodeCoverageSummary;

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
  run("npm", ["run", "test:ui:rust"], {
    ...instrumentedEnvironment,
    ATTYD_RUST_BINARY: rustBinary,
    ATTYD_SMOKE_SKIP_OVERSIZED_LINE: "1",
  });
  run("npm", ["run", "test:rust:remote"], {
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
    rustReportPath,
  ]);
  run("cargo", ["llvm-cov", "report", "--offline"]);
  const rustSummary = JSON.parse(
    await readFile(rustReportPath, "utf8"),
  ) as RustCoverageSummary;

  const nodeLines = nodeSummary.total.lines.pct;
  const rustLines = rustSummary.data[0]?.totals.lines.percent;
  if (rustLines == null || !Number.isFinite(rustLines)) {
    throw new Error("Rust coverage report did not contain a finite line percentage");
  }

  console.log("\nBackend line coverage parity");
  console.log(`  Node server/shared: ${nodeLines.toFixed(2)}%`);
  console.log(`  Rust backend:       ${rustLines.toFixed(2)}%`);
  if (rustLines + Number.EPSILON < nodeLines) {
    throw new Error(
      `Rust backend line coverage ${rustLines.toFixed(2)}% is below Node ${nodeLines.toFixed(2)}%`,
    );
  }
  console.log("  Result: PASS (Rust is at or above the Node line-coverage baseline)");
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
