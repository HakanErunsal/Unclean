import { execFileSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { extname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const root = resolve(fileURLToPath(new URL("..", import.meta.url)));
const skippedFiles = new Set([
  "Cargo.lock",
  "LICENSE-APACHE",
  "LICENSE-MIT",
  "scripts/check-foundation.mjs",
  "scripts/check-public-text.mjs"
]);
const checkedExtensions = new Set([
  ".hbs",
  ".html",
  ".md",
  ".mjs",
  ".ps1",
  ".rs",
  ".toml",
  ".uplugin",
  ".yaml",
  ".yml"
]);
const prohibitedWords = [
  "actually",
  "crucially",
  "deeply",
  "fundamentally",
  "genuinely",
  "honestly",
  "importantly",
  "inevitably",
  "inherently",
  "interestingly",
  "just",
  "literally",
  "powerful",
  "really",
  "robust",
  "seamlessly",
  "simply",
  "truly"
];
const stockPhrases = [
  "at its core",
  "at the end of the day",
  "can we talk about",
  "circle back",
  "deep dive",
  "double down",
  "full stop",
  "here's the problem",
  "here's the thing",
  "here's what",
  "here's why",
  "in conclusion",
  "it is important",
  "it turns out",
  "let that sink in",
  "let me be clear",
  "make no mistake",
  "moving forward",
  "not just",
  "note that",
  "on the same page",
  "serves as",
  "stands as",
  "take a step back",
  "the fact that",
  "the reality is",
  "the truth is",
  "when it comes to"
];
let failureCount = 0;

function lineNumber(text, index) {
  return text.slice(0, index).split("\n").length;
}

function report(condition, failure, action) {
  if (condition) return;
  failureCount += 1;
  console.error(`${failure} ${action}`);
}

function extractMarkdown(text) {
  return text
    .replace(/```[\s\S]*?```/g, (match) => match.replace(/[^\n]/g, " "))
    .replace(/`[^`\n]+`/g, (match) => " ".repeat(match.length));
}

function extractHtml(text) {
  return text
    .replace(/<style[\s\S]*?<\/style>/gi, (match) => match.replace(/[^\n]/g, " "))
    .replace(/<script[\s\S]*?<\/script>/gi, (match) => match.replace(/[^\n]/g, " "))
    .replace(/<[^>]+>/g, (match) => " ".repeat(match.length));
}

function extractYaml(text) {
  return text
    .split("\n")
    .map((line) => line.replace(/^\s*-\s+/, "").replace(/^\s*[A-Za-z0-9_-]+:\s*/, ""))
    .join("\n");
}

function extractCodeText(text) {
  const chunks = [];
  const pattern = /\/\/[^\n]*|\/\*[\s\S]*?\*\/|r#*"[\s\S]*?"#*|"(?:\\.|[^"\\])*"/g;
  for (const match of text.matchAll(pattern)) {
    chunks.push({ index: match.index, text: match[0] });
  }
  return chunks;
}

function checkText(path, source, text, offset = 0) {
  for (const word of prohibitedWords) {
    const match = new RegExp(`\\b${word}\\b`, "i").exec(text);
    report(
      !match,
      `Public text uses prohibited word "${word}" in ${path}${match ? `:${lineNumber(source, offset + match.index)}` : ""}.`,
      "Replace it with the concrete behavior or action."
    );
  }

  for (const phrase of stockPhrases) {
    const match = new RegExp(`\\b${phrase}\\b`, "i").exec(text);
    report(
      !match,
      `Public text uses stock phrase "${phrase}" in ${path}${match ? `:${lineNumber(source, offset + match.index)}` : ""}.`,
      "State the behavior or decision directly."
    );
  }

  const emDash = text.indexOf("—");
  report(
    emDash === -1,
    `Public text uses an em dash in ${path}${emDash >= 0 ? `:${lineNumber(source, offset + emDash)}` : ""}.`,
    "Split the sentence or state the relationship with direct punctuation."
  );

  const connector = text.indexOf(" - ");
  report(
    connector === -1,
    `Public text uses a spaced-hyphen connector in ${path}${connector >= 0 ? `:${lineNumber(source, offset + connector)}` : ""}.`,
    "Split the sentence or state the relationship with direct punctuation."
  );
}

const trackedFiles = execFileSync("git", ["ls-files", "-z"], { cwd: root })
  .toString()
  .split("\0")
  .filter(Boolean);

for (const trackedPath of trackedFiles) {
  const absolutePath = resolve(root, trackedPath);
  const path = relative(root, absolutePath).replaceAll("\\", "/");
  if (skippedFiles.has(path) || !checkedExtensions.has(extname(path))) continue;
  const source = readFileSync(absolutePath, "utf8");
  const extension = extname(path);

  if (extension === ".rs" || extension === ".mjs") {
    const chunks = extractCodeText(source);
    for (const chunk of chunks) {
      checkText(path, source, chunk.text, chunk.index);
      if (chunk.text.startsWith("//") || chunk.text.startsWith("/*")) {
        const pronoun = /\b(?:I|we|you|your)\b/i.exec(chunk.text);
        report(
          !pronoun,
          `Code comment uses a personal pronoun in ${path}${pronoun ? `:${lineNumber(source, chunk.index)}` : ""}.`,
          "Describe the current behavior in neutral language."
        );
      }
    }
    const blockComment = source.indexOf("/*");
    report(
      blockComment === -1,
      `Code uses a block comment in ${path}${blockComment >= 0 ? `:${lineNumber(source, blockComment)}` : ""}.`,
      "Use one physical line for each comment."
    );
    continue;
  }

  if (extension === ".md") {
    checkText(path, source, extractMarkdown(source));
    continue;
  }

  if (extension === ".html") {
    checkText(path, source, extractHtml(source));
    continue;
  }

  if (extension === ".yaml" || extension === ".yml") {
    checkText(path, source, extractYaml(source));
    continue;
  }

  checkText(path, source, source);
}

if (failureCount > 0) {
  console.error(`Public text check failed with ${failureCount} finding${failureCount === 1 ? "" : "s"}.`);
  process.exit(1);
}

console.log("Public text checks passed.");
