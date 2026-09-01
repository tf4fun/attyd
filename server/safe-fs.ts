import { createReadStream, realpathSync } from "node:fs";
import { readFile, readdir, realpath, stat, writeFile } from "node:fs/promises";
import { basename, dirname, extname, isAbsolute, join, relative, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import {
  RequestError,
  type ReadTextFileRequest,
  type ReadTextFileResponse,
  type WriteTextFileRequest,
  type WriteTextFileResponse,
} from "@agentclientprotocol/sdk";
import type {
  WorkspaceContextAttachment,
  WorkspaceContextMatch,
} from "../shared/bridge.js";

const MAX_FILE_CONTENT_BYTES = 4_000_000;
const MAX_FILE_SCAN_BYTES = 128_000_000;
const MAX_FILE_PATH_LENGTH = 16_384;
const MAX_LINE_NUMBER = 4_294_967_295;
const MAX_CONTEXT_BYTES = 3 * 1024 * 1024;
const MAX_CONTEXT_QUERY_LENGTH = 256;
const MAX_CONTEXT_RESULTS = 24;
const MAX_CONTEXT_SCAN_ENTRIES = 50_000;
const MAX_CONTEXT_DEPTH = 32;
const IGNORED_CONTEXT_DIRECTORIES = new Set([
  ".git",
  ".hg",
  ".svn",
  ".cache",
  ".next",
  ".turbo",
  "build",
  "coverage",
  "dist",
  "node_modules",
  "target",
  "vendor",
]);
const CONTEXT_TEXT_EXTENSIONS = new Set([
  ".c", ".cc", ".clj", ".cljs", ".cmake", ".cpp", ".cs", ".css", ".csv",
  ".dart", ".ex", ".exs", ".go", ".graphql", ".h", ".hpp", ".html", ".java",
  ".js", ".json", ".jsx", ".kt", ".kts", ".less", ".lua", ".md", ".mdx",
  ".mjs", ".php", ".proto", ".py", ".r", ".rb", ".rs", ".sass", ".scala",
  ".scss", ".sh", ".sql", ".svelte", ".swift", ".toml", ".ts", ".tsx", ".txt",
  ".vue", ".xml", ".yaml", ".yml", ".zig",
]);

export class WorkspaceFileSystem {
  private readonly root: string;
  private readonly roots: Array<{ lexical: string; real: string }>;

  constructor(
    root: string,
    private readonly readOnly: boolean,
    additionalRoots: string[] = [],
  ) {
    this.roots = [...new Set([root, ...additionalRoots])].map((candidate) => {
      const lexical = resolve(candidate);
      return { lexical, real: realpathSync(lexical) };
    });
    this.root = this.roots[0].real;
  }

  async read(
    params: ReadTextFileRequest,
    signal?: AbortSignal,
  ): Promise<ReadTextFileResponse> {
    try {
      validateReadRange(params);
      throwIfCancelled(signal);
      const path = await this.checkedExistingPath(params.path);
      throwIfCancelled(signal);
      if (params.line != null || params.limit != null) {
        return {
          content: await readLineRange(path, params.line ?? 1, params.limit, signal),
        };
      }
      const info = await stat(path);
      throwIfCancelled(signal);
      if (info.size > MAX_FILE_CONTENT_BYTES) {
        throw new Error(`ACP file read exceeds ${MAX_FILE_CONTENT_BYTES} bytes`);
      }
      const text = await readFile(path, { encoding: "utf8", signal });
      assertContentSize(text, "read");
      return { content: text };
    } catch (error) {
      if (signal?.aborted && !(error instanceof RequestError)) {
        throw RequestError.requestCancelled();
      }
      throw error;
    }
  }

  async write(
    params: WriteTextFileRequest,
    signal?: AbortSignal,
  ): Promise<WriteTextFileResponse> {
    if (this.readOnly) throw new Error("attyd is running in read-only mode");
    assertContentSize(params.content, "write");
    throwIfCancelled(signal);
    const path = this.checkedLexicalPath(params.path);
    let writeTarget: string;
    try {
      const existingTarget = await realpath(path);
      this.assertWithin(existingTarget);
      writeTarget = existingTarget;
    } catch (error) {
      if (!isMissing(error)) throw error;
      const realParent = await realpath(dirname(path));
      this.assertWithin(realParent);
      writeTarget = join(realParent, basename(path));
    }
    // Once the write starts, finish it and return a normal result even if the
    // peer cancels; aborting an in-place write could leave a partial file.
    throwIfCancelled(signal);
    await writeFile(writeTarget, params.content, "utf8");
    return {};
  }

  async searchContext(query: string): Promise<WorkspaceContextMatch[]> {
    if (query.length > MAX_CONTEXT_QUERY_LENGTH || query.includes("\0")) {
      throw new Error(
        `Context search query must be at most ${MAX_CONTEXT_QUERY_LENGTH} characters without NUL bytes`,
      );
    }
    const normalizedQuery = query.trim().toLocaleLowerCase();
    const terms = normalizedQuery.split(/[\\/\s]+/).filter(Boolean);
    const candidates: Array<{
      path: string;
      relativePath: string;
      rootName: string;
      score: number;
    }> = [];
    let scanned = 0;

    for (const root of this.roots) {
      const rootName = basename(root.real) || root.real;
      const pending = [{ directory: root.real, relativePath: "", depth: 0 }];
      while (pending.length > 0 && scanned < MAX_CONTEXT_SCAN_ENTRIES) {
        const current = pending.shift();
        if (!current) break;
        let entries;
        try {
          entries = await readdir(current.directory, { withFileTypes: true });
        } catch {
          continue;
        }
        entries.sort((left, right) => left.name.localeCompare(right.name));
        for (const entry of entries) {
          scanned += 1;
          if (scanned > MAX_CONTEXT_SCAN_ENTRIES) break;
          const relativePath = current.relativePath
            ? join(current.relativePath, entry.name)
            : entry.name;
          if (entry.isDirectory()) {
            if (
              current.depth < MAX_CONTEXT_DEPTH &&
              !IGNORED_CONTEXT_DIRECTORIES.has(entry.name)
            ) {
              pending.push({
                directory: join(current.directory, entry.name),
                relativePath,
                depth: current.depth + 1,
              });
            }
            continue;
          }
          if (!entry.isFile() || !isContextTextPath(relativePath)) continue;
          const score = contextMatchScore(relativePath, normalizedQuery, terms);
          if (!Number.isFinite(score)) continue;
          candidates.push({
            path: join(current.directory, entry.name),
            relativePath,
            rootName,
            score,
          });
        }
      }
      if (scanned >= MAX_CONTEXT_SCAN_ENTRIES) break;
    }

    candidates.sort((left, right) =>
      left.score - right.score ||
      left.relativePath.localeCompare(right.relativePath) ||
      left.rootName.localeCompare(right.rootName)
    );
    const matches: WorkspaceContextMatch[] = [];
    for (const candidate of candidates) {
      if (matches.length >= MAX_CONTEXT_RESULTS) break;
      let info;
      try {
        info = await stat(candidate.path);
      } catch {
        continue;
      }
      if (!info.isFile() || info.size > MAX_CONTEXT_BYTES) continue;
      matches.push({
        path: candidate.path,
        name: basename(candidate.path),
        relativePath: candidate.relativePath,
        rootName: candidate.rootName,
        size: info.size,
      });
    }
    return matches;
  }

  async readContext(path: string): Promise<WorkspaceContextAttachment> {
    const target = await this.checkedExistingPath(path);
    const info = await stat(target);
    if (!info.isFile()) throw new Error("Workspace context must be a regular file");
    if (!isContextTextPath(target)) {
      throw new Error("Workspace context is not a supported text file");
    }
    if (info.size > MAX_CONTEXT_BYTES) {
      throw new Error(`Workspace context exceeds ${MAX_CONTEXT_BYTES} bytes`);
    }
    const text = await readFile(target, "utf8");
    const size = Buffer.byteLength(text, "utf8");
    if (size > MAX_CONTEXT_BYTES) {
      throw new Error(`Workspace context exceeds ${MAX_CONTEXT_BYTES} bytes`);
    }
    if (text.includes("\0")) throw new Error("Workspace context is not a text file");
    return {
      name: this.contextDisplayPath(target),
      size,
      block: {
        type: "resource",
        resource: {
          uri: pathToFileURL(target).href,
          mimeType: contextMimeType(target),
          text,
        },
      },
    };
  }

  async checkedDirectory(path: string | null | undefined): Promise<string> {
    const target = path == null ? this.root : this.checkedLexicalPath(path);
    const realTarget = await realpath(target);
    this.assertWithin(realTarget);
    return realTarget;
  }

  private async checkedExistingPath(path: string): Promise<string> {
    const lexical = this.checkedLexicalPath(path);
    const target = await realpath(lexical);
    this.assertWithin(target);
    return target;
  }

  private checkedLexicalPath(path: string): string {
    if (
      path.length === 0 ||
      path.length > MAX_FILE_PATH_LENGTH ||
      path.includes("\0")
    ) {
      throw new Error(
        `ACP filesystem path must contain between 1 and ${MAX_FILE_PATH_LENGTH} characters without NUL bytes`,
      );
    }
    if (!isAbsolute(path)) {
      throw new Error(`ACP filesystem paths must be absolute: ${path}`);
    }
    const target = resolve(path);
    if (!this.roots.some(({ lexical, real }) => isWithin(lexical, target) || isWithin(real, target))) {
      throw new Error(`Path is outside the workspace boundary: ${target}`);
    }
    return target;
  }

  private assertWithin(path: string): void {
    if (this.roots.some(({ real }) => isWithin(real, path))) return;
    throw new Error(`Path is outside the workspace boundary: ${path}`);
  }

  private contextDisplayPath(path: string): string {
    const root = this.roots.find(({ real }) => isWithin(real, path));
    if (!root) return basename(path);
    const child = relative(root.real, path);
    return root.real === this.root ? child : join(basename(root.real), child);
  }
}

function contextMatchScore(path: string, query: string, terms: string[]): number {
  const normalizedPath = path.replaceAll("\\", "/").toLocaleLowerCase();
  const name = basename(normalizedPath);
  if (!terms.every((term) => normalizedPath.includes(term))) {
    return Number.POSITIVE_INFINITY;
  }
  if (!query) return normalizedPath.split("/").length * 100 + normalizedPath.length;
  if (normalizedPath === query || name === query) return 0;
  if (name.startsWith(query)) return 10 + name.length;
  if (name.includes(query)) return 100 + name.indexOf(query) * 4 + name.length;
  if (normalizedPath.startsWith(query)) return 500 + normalizedPath.length;
  const index = normalizedPath.indexOf(query);
  if (index >= 0) return 1_000 + index * 4 + normalizedPath.length;
  return 2_000 + normalizedPath.length;
}

function isContextTextPath(path: string): boolean {
  const extension = extname(path).toLocaleLowerCase();
  if (CONTEXT_TEXT_EXTENSIONS.has(extension)) return true;
  const name = basename(path).toLocaleLowerCase();
  return [
    ".env",
    ".gitignore",
    ".npmrc",
    "dockerfile",
    "gemfile",
    "justfile",
    "makefile",
    "readme",
  ].some((candidate) => name === candidate || name.startsWith(`${candidate}.`));
}

function contextMimeType(path: string): string {
  const extension = extname(path).toLocaleLowerCase();
  if (extension === ".json") return "application/json";
  if (extension === ".xml") return "application/xml";
  if (extension === ".html") return "text/html";
  if (extension === ".css") return "text/css";
  if ([".js", ".mjs", ".jsx"].includes(extension)) return "text/javascript";
  if ([".ts", ".tsx"].includes(extension)) return "text/typescript";
  if ([".yaml", ".yml"].includes(extension)) return "application/yaml";
  if (extension === ".md" || extension === ".mdx") return "text/markdown";
  return "text/plain";
}

function validateReadRange(params: ReadTextFileRequest): void {
  if (
    params.line != null &&
    (!Number.isSafeInteger(params.line) || params.line < 1 || params.line > MAX_LINE_NUMBER)
  ) {
    throw new Error("ACP file read line must be a 1-based uint32 integer");
  }
  if (
    params.limit != null &&
    (!Number.isSafeInteger(params.limit) || params.limit < 0 || params.limit > MAX_LINE_NUMBER)
  ) {
    throw new Error("ACP file read limit must be a uint32 integer");
  }
}

function assertContentSize(content: string, operation: "read" | "write"): void {
  if (Buffer.byteLength(content, "utf8") > MAX_FILE_CONTENT_BYTES) {
    throw new Error(`ACP file ${operation} exceeds ${MAX_FILE_CONTENT_BYTES} bytes`);
  }
}

async function readLineRange(
  path: string,
  startLine: number,
  limit: number | null | undefined,
  signal?: AbortSignal,
): Promise<string> {
  if (limit === 0) return "";
  const maximumLines = limit ?? Number.POSITIVE_INFINITY;
  const stream = createReadStream(path, { encoding: "utf8", signal });
  let output = "";
  let outputBytes = 0;
  let scannedBytes = 0;
  let currentLine = 1;
  let selectedLines = 0;

  const append = (text: string) => {
    output += text;
    outputBytes += Buffer.byteLength(text, "utf8");
    if (outputBytes > MAX_FILE_CONTENT_BYTES) {
      throw new Error(`ACP file read exceeds ${MAX_FILE_CONTENT_BYTES} bytes`);
    }
  };

  try {
    for await (const rawChunk of stream) {
      const chunk = String(rawChunk);
      scannedBytes += Buffer.byteLength(chunk, "utf8");
      if (scannedBytes > MAX_FILE_SCAN_BYTES) {
        throw new Error(`ACP file range scan exceeds ${MAX_FILE_SCAN_BYTES} bytes`);
      }

      let offset = 0;
      for (;;) {
        const newline = chunk.indexOf("\n", offset);
        const end = newline < 0 ? chunk.length : newline;
        if (currentLine >= startLine && selectedLines < maximumLines) {
          append(chunk.slice(offset, end));
        }
        if (newline < 0) break;

        if (currentLine >= startLine && selectedLines < maximumLines) {
          if (output.endsWith("\r")) {
            output = output.slice(0, -1);
            outputBytes -= 1;
          }
          selectedLines += 1;
          if (selectedLines >= maximumLines) return output;
          append("\n");
        }
        currentLine += 1;
        offset = newline + 1;
      }
    }
    return output;
  } finally {
    stream.destroy();
  }
}

function throwIfCancelled(signal: AbortSignal | undefined): void {
  if (signal?.aborted) throw RequestError.requestCancelled();
}

function isWithin(root: string, path: string): boolean {
  const offset = relative(root, path);
  return offset === "" || (!offset.startsWith("..") && !isAbsolute(offset));
}

function isMissing(error: unknown): boolean {
  return (
    typeof error === "object" &&
    error !== null &&
    "code" in error &&
    error.code === "ENOENT"
  );
}
