#!/usr/bin/env node
import { parseOptions } from "./options.js";
import { startHttpServer } from "./http-server.js";

try {
  await startHttpServer(parseOptions(process.argv.slice(2)));
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
