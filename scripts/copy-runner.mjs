import { copyFileSync, mkdirSync, existsSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = dirname(fileURLToPath(import.meta.url));
const root = join(__dirname, "..");
const src = join(root, "target", "release", "dqc-runner.exe");
const destDir = join(root, "src-tauri", "runner");
const dest = join(destDir, "dqc-runner.exe");

if (!existsSync(src)) {
  console.error(
    `Runner binary not found at ${src}\nRun "npm run build:runner:win" first.`
  );
  process.exit(1);
}

mkdirSync(destDir, { recursive: true });
copyFileSync(src, dest);
console.log(`Copied runner -> ${dest}`);
