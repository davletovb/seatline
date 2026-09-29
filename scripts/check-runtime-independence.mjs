#!/usr/bin/env node
import { execFileSync } from "node:child_process";

const manifests = ["Cargo.toml", "seatline-fuzz/Cargo.toml"];
const expected = new Set([
  "seatline-core",
  "seatline-platform",
  "seatline-providers",
  "seatline-scheduler",
  "seatline-service",
  "seatline-fake-provider",
  "seatline-tests",
  "seatline-fuzz",
]);

const packages = manifests.flatMap((manifest) => {
  const out = execFileSync(
    "cargo",
    ["metadata", "--format-version", "1", "--no-deps", "--locked", "--manifest-path", manifest],
    { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] },
  );
  return JSON.parse(out).packages;
});

const names = new Set(packages.map((pkg) => pkg.name));
const missing = [...expected].filter((name) => !names.has(name));
const foreign = [...names].filter((name) => !name.startsWith("seatline-"));

if (missing.length || foreign.length) {
  if (missing.length) console.error(`Expected Seatline crates not found: ${missing.join(", ")}`);
  if (foreign.length) console.error(`Non-Seatline workspace crates found: ${foreign.join(", ")}`);
  process.exit(1);
}

for (const pkg of packages) {
  for (const dep of pkg.dependencies) {
    if (dep.path && !dep.name.startsWith("seatline-")) {
      console.error(`${pkg.name} has a local dependency outside Seatline: ${dep.name}`);
      process.exit(1);
    }
  }
}

console.log(`Seatline workspace boundary OK: ${expected.size} expected crates, no application crates.`);
