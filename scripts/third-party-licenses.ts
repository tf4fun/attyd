import { execFileSync } from "node:child_process";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { dirname, join, relative, resolve } from "node:path";

interface CargoPackage {
  id: string;
  name: string;
  version: string;
  license: string | null;
  license_file: string | null;
  repository: string | null;
  authors: string[];
  manifest_path: string;
  source: string | null;
}

interface CargoMetadata {
  packages: CargoPackage[];
  resolve: {
    root: string;
    nodes: Array<{
      id: string;
      deps: Array<{ pkg: string; dep_kinds: Array<{ kind: string | null }> }>;
    }>;
  };
}

interface NpmPackage {
  version: string;
  license?: string;
  dev?: boolean;
}

const workspace = process.cwd();
const target = process.argv[2] ?? execFileSync("rustc", ["-vV"], { encoding: "utf8" })
  .match(/^host: (.+)$/mu)?.[1];
if (!target) throw new Error("Pass the Rust target triple as the first argument");
const output = resolve(process.argv[3] ?? `target/${target}/release/THIRD_PARTY_LICENSES.txt`);
const metadata = JSON.parse(execFileSync("cargo", [
  "metadata", "--locked", "--format-version", "1", "--filter-platform", target,
], { encoding: "utf8", maxBuffer: 32 * 1024 * 1024 })) as CargoMetadata;
const nodes = new Map(metadata.resolve.nodes.map((node) => [node.id, node]));
const included = new Set<string>();
const pending = [metadata.resolve.root];
while (pending.length) {
  const id = pending.pop()!;
  if (included.has(id)) continue;
  included.add(id);
  for (const dependency of nodes.get(id)?.deps ?? []) {
    if (dependency.dep_kinds.some((entry) => entry.kind !== "dev")) pending.push(dependency.pkg);
  }
}

// These published crates omit license files and explicitly offer Apache-2.0.
// Keep this list version-specific so a dependency change requires review.
const apacheFallbacks = new Set([
  "agent-client-protocol-schema@1.7.0",
  "defmt-parser@1.0.0",
  "eventsource-stream@0.2.3",
  "include-flate-codegen@0.3.4",
  "include-flate-compress@0.3.4",
]);
const sections = [
  "Third-party licenses for attyd",
  `Rust target: ${target}`,
  "Generated from Cargo.lock and package-lock.json. Includes Rust runtime/build",
  "dependencies and production npm packages; some listed code may be compiled out.",
  "Original license and notice files follow. Dependency licenses remain their own.",
  "",
];

for (const pkg of metadata.packages.sort((a, b) => a.id.localeCompare(b.id))) {
  if (!included.has(pkg.id) || pkg.id === metadata.resolve.root) continue;
  const root = dirname(pkg.manifest_path);
  const files = await licenseFiles(root);
  if (pkg.license_file) files.push(resolve(root, pkg.license_file));
  const name = `${pkg.name}@${pkg.version}`;
  if (pkg.source?.startsWith("git+https://github.com/agentclientprotocol/rust-sdk.git")) {
    files.push(resolve(root, "../../LICENSE"));
  }
  if (name === "alloc-stdlib@0.2.4") {
    files.push(resolve("licenses/alloc-stdlib-0.2.4.txt"));
  }
  if (files.length === 0 && apacheFallbacks.has(name)) {
    if (pkg.license !== "Apache-2.0" && pkg.license !== "MIT OR Apache-2.0") {
      throw new Error(`Review changed Apache-2.0 license declaration for ${name}`);
    }
    files.push(resolve("LICENSE"));
  }
  await appendPackage(`Rust: ${name}`, pkg.license, pkg.repository, pkg.authors, root, files);
}

const lock = JSON.parse(await readFile("package-lock.json", "utf8")) as {
  packages: Record<string, NpmPackage>;
};
for (const [path, pkg] of Object.entries(lock.packages).sort(([a], [b]) => a.localeCompare(b))) {
  if (!path || pkg.dev) continue;
  const root = resolve(path);
  const packageJson = JSON.parse(await readFile(join(root, "package.json"), "utf8")) as {
    name: string;
    version: string;
    homepage?: string;
  };
  if (packageJson.version !== pkg.version) throw new Error(`Run npm ci: version mismatch for ${path}`);
  await appendPackage(`npm: ${packageJson.name}@${pkg.version}`, pkg.license,
    packageJson.homepage, [], root, await licenseFiles(root));
}

await mkdir(dirname(output), { recursive: true });
await writeFile(output, sections.join("\n"));
console.log(`Wrote third-party license materials to ${relative(workspace, output)}`);

async function appendPackage(
  name: string,
  license: string | null | undefined,
  repository: string | null | undefined,
  authors: string[],
  root: string,
  files: string[],
) {
  if (!license || files.length === 0) throw new Error(`Review missing license materials for ${name}`);
  sections.push("=".repeat(80), name, `Declared license: ${license}`);
  if (repository) sections.push(`Source: ${repository}`);
  if (authors.length) sections.push(`Authors: ${authors.join(", ")}`);
  for (const file of [...new Set(files)].sort()) {
    const label = file.startsWith(root + "/") ? relative(root, file) : relative(workspace, file);
    if (label.startsWith("../")) {
      // Cargo's SDK workspace license is outside its member crate, but no local path is published.
      sections.push("\n--- upstream workspace LICENSE ---\n");
    } else {
      sections.push(`\n--- ${label} ---\n`);
    }
    sections.push((await readFile(file, "utf8")).trimEnd(), "");
  }
}

async function licenseFiles(root: string): Promise<string[]> {
  const result: string[] = [];
  for (const entry of await readdir(root, { withFileTypes: true })) {
    if (["node_modules", ".git", "target"].includes(entry.name)) continue;
    const path = join(root, entry.name);
    if (entry.isDirectory()) result.push(...await licenseFiles(path));
    else if (entry.isFile() && /^(licen[sc]e|copying|copyright|notice)([._-]|$)/iu.test(entry.name)) {
      result.push(path);
    }
  }
  return result;
}
