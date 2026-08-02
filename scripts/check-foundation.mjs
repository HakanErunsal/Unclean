import { execFileSync } from "node:child_process";
import { existsSync, readFileSync } from "node:fs";
import { dirname, extname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(dirname(fileURLToPath(import.meta.url)), "..");
let failureCount = 0;

function report(condition, failure, action) {
  if (condition) return;
  failureCount += 1;
  console.error(`${failure} ${action}`);
}

function read(path) {
  return readFileSync(resolve(root, path), "utf8");
}

function lineNumber(text, index) {
  return text.slice(0, index).split("\n").length;
}

function trackedFiles() {
  return execFileSync("git", ["ls-files", "-z"], { cwd: root })
    .toString()
    .split("\0")
    .filter(Boolean);
}

const requiredPaths = [
  ".gitattributes",
  "Cargo.toml",
  "LICENSE-APACHE",
  "LICENSE-MIT",
  "README.md",
  "RELEASE_NOTES.md",
  "CONTRIBUTING.md",
  "SECURITY.md",
  "PRIVACY.md",
  "CODE_OF_CONDUCT.md",
  "deny.toml",
  ".github/workflows/release.yml",
  "about.toml",
  "about.hbs",
  "presets/windows-desktop-lean.toml",
  "scripts/package-release.ps1",
  "scripts/run-installed-engine-acceptance.ps1",
  "tests/fixtures/descriptors/cases.toml",
  "tests/fixtures/descriptors/utf8-bom-crlf-enabled.hex"
];

for (const path of requiredPaths) {
  report(existsSync(resolve(root, path)), `Required file is missing: ${path}.`, "Add the file and run this check again.");
}

const cargoManifest = read("Cargo.toml");
report(
  /license\s*=\s*"MIT OR Apache-2\.0"/.test(cargoManifest),
  "Cargo license metadata is missing.",
  "Set the workspace license to MIT OR Apache-2.0."
);
report(
  /repository\s*=\s*"https:\/\/github\.com\/HakanErunsal\/Unclean"/.test(cargoManifest),
  "Cargo repository metadata is missing.",
  "Set the workspace repository to the public GitHub URL."
);

const releaseWorkflow = read(".github/workflows/release.yml");
const releasePackage = read("scripts/package-release.ps1");
report(
  /actions\/checkout@[0-9a-f]{40}/.test(releaseWorkflow) &&
    /actions\/attest@[0-9a-f]{40}/.test(releaseWorkflow) &&
    /actions\/upload-artifact@[0-9a-f]{40}/.test(releaseWorkflow),
  "Release workflow actions are not pinned to full revisions.",
  "Pin every release action to a reviewed 40-character commit."
);
report(
  /subject-path:/.test(releaseWorkflow) &&
    /sbom-path:/.test(releaseWorkflow) &&
    /\.sha256/.test(releaseWorkflow) &&
    /gh release create/.test(releaseWorkflow),
  "Release workflow is missing integrity or publication steps.",
  "Generate checksums, attest the SBOMs, and publish only from a tag."
);
report(
  /--notes-file RELEASE_NOTES\.md/.test(releaseWorkflow) &&
    /--prerelease/.test(releaseWorkflow) &&
    /--draft/.test(releaseWorkflow),
  "Release workflow does not preserve the reviewed pre-release notes.",
  "Publish the reviewed notes from a draft pre-release."
);
report(
  !/docs[\\/]/.test(releasePackage),
  "Release package references the private docs directory.",
  "Package tracked public files only."
);

const markdownFiles = trackedFiles()
  .filter((path) => extname(path) === ".md")
  .map((path) => resolve(root, path));
const linkPattern = /!?\[[^\]]*]\(([^)]+)\)/g;

for (const path of markdownFiles) {
  const text = readFileSync(path, "utf8");
  for (const match of text.matchAll(linkPattern)) {
    const rawTarget = match[1].trim().replace(/^<|>$/g, "");
    if (/^(?:https?:|mailto:|#)/.test(rawTarget)) continue;
    const target = decodeURIComponent(rawTarget.split("#", 1)[0]);
    const resolvedTarget = resolve(dirname(path), target);
    report(
      existsSync(resolvedTarget),
      `Broken relative link in ${relative(root, path)}:${lineNumber(text, match.index)} points to ${target}.`,
      "Correct the target or add the referenced file."
    );
  }
}

const prohibitedWords = [
  "simply",
  "just",
  "really",
  "actually",
  "seamlessly",
  "powerful",
  "robust",
  "game-changer"
];

for (const path of markdownFiles) {
  const projectPath = relative(root, path).replaceAll("\\", "/");
  const text = readFileSync(path, "utf8").replace(/```[\s\S]*?```/g, "");
  for (const word of prohibitedWords) {
    const match = new RegExp(`\\b${word}\\b`, "i").exec(text);
    report(
      !match,
      `Public text uses prohibited word "${word}" in ${projectPath}${match ? `:${lineNumber(text, match.index)}` : ""}.`,
      "Replace it with the concrete behavior or action."
    );
  }
  const emDash = text.indexOf("—");
  report(
    emDash === -1,
    `Public text uses an em dash in ${projectPath}${emDash >= 0 ? `:${lineNumber(text, emDash)}` : ""}.`,
    "Split the sentence or use punctuation that states the relationship directly."
  );
  const connector = text.indexOf(" - ");
  report(
    connector === -1,
    `Public text uses a spaced-hyphen connector in ${projectPath}${connector >= 0 ? `:${lineNumber(text, connector)}` : ""}.`,
    "Split the sentence or use punctuation that states the relationship directly."
  );
}

const fixtureHex = read("tests/fixtures/descriptors/utf8-bom-crlf-enabled.hex").trim();
report(
  /^[0-9a-f]+$/.test(fixtureHex) && fixtureHex.length % 2 === 0,
  "The byte-sensitive fixture is not valid lowercase hexadecimal.",
  "Store one complete lowercase hexadecimal byte sequence."
);
const fixtureBytes = Buffer.from(fixtureHex, "hex");
report(
  fixtureBytes.subarray(0, 3).equals(Buffer.from([0xef, 0xbb, 0xbf])),
  "The byte-sensitive fixture has no UTF-8 byte-order mark.",
  "Restore the expected EF BB BF prefix."
);
const fixtureText = fixtureBytes.subarray(3).toString("utf8");
report(
  fixtureText.includes("\r\n") && !/(?<!\r)\n/.test(fixtureText),
  "The byte-sensitive fixture contains an unexpected line ending.",
  "Encode every fixture line ending as CRLF."
);

if (failureCount > 0) {
  console.error(`Repository foundation check failed with ${failureCount} finding${failureCount === 1 ? "" : "s"}.`);
  process.exit(1);
}

console.log("Repository foundation checks passed.");
