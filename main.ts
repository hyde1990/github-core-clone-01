// Thin launcher: the Rust binary does all the work. Node never sits on
// a hot path — it only spawns ./forge beside this file, mirrors its exit
// code, and passes the harness environment through.
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const dir = dirname(fileURLToPath(import.meta.url));
const binary = join(dir, "forge");

const child = spawn(binary, [], {
  stdio: "inherit",
  env: {
    ...process.env,
    // Pass the harness environment through unchanged.
    MAKEFUN_ENV_URL: process.env.MAKEFUN_ENV_URL ?? "",
    MAKEFUN_SHOP_TOKEN: process.env.MAKEFUN_SHOP_TOKEN ?? "",
  },
});

child.on("error", (error) => {
  console.error(`forge: failed to start ${binary}: ${error.message}`);
  process.exit(1);
});

child.on("exit", (code, signal) => {
  if (signal) {
    process.kill(process.pid, signal);
    return;
  }
  process.exit(code ?? 0);
});
