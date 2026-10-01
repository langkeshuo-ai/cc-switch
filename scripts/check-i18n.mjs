#!/usr/bin/env node
/**
 * i18n 键完整性护栏（防"键改名但 JSON 未迁移"事故复发，如
 * settings.advanced.snapshot.* → settings.snapshot.* 漂移）。
 *
 * 检查项：
 *   1. 漏键（阻断）：代码中 t("key") 字面量引用但 en.json 缺失。
 *   2. 四语言键集一致性（阻断）：en 有而 zh/ja/zh-TW 缺失的键。
 *   3. 死键（仅报告，不阻断）：en.json 定义但代码零引用。
 *
 * 动态键（模板字符串拼接）无法静态解析，通过 DYNAMIC_KEY_PREFIXES 前缀
 * 白名单豁免。新增动态键命名空间时必须同步维护此清单。
 */
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const srcDir = join(root, "src");
const localesDir = join(root, "src", "i18n", "locales");

/** 动态键命名空间前缀（模板字符串拼接，静态扫描不可见） */
const DYNAMIC_KEY_PREFIXES = [
  "apps.",
  "profiles.switcherTooltip.",
  "pi.form.thinkingLevels.",
  "providerForm.partnerPromotion.",
  "settings.terminal.options.",
  "settings.snapshot.skipReason.",
  "usage.appFilter.",
  "skills.error.",
  "errors.",
];

/** t() 的常见别名（新增封装时同步维护） */
const T_FUNCS = /\b(?:t|i18nKey)\(\s*(['"])((?:(?!\1)[^\\])*)\1/g;

function* walk(dir) {
  for (const name of readdirSync(dir)) {
    const p = join(dir, name);
    const s = statSync(p);
    if (s.isDirectory()) yield* walk(p);
    else if (/\.(tsx?|jsx?)$/.test(name)) yield p;
  }
}

function flatten(obj, prefix = "", out = new Set()) {
  for (const [k, v] of Object.entries(obj)) {
    const key = prefix ? `${prefix}.${k}` : k;
    if (v && typeof v === "object" && !Array.isArray(v)) flatten(v, key, out);
    else out.add(key);
  }
  return out;
}

// ---- 收集代码引用键 ----
const usedKeys = new Set();
for (const file of walk(srcDir)) {
  if (file.includes(`${"locales"}`)) continue;
  const text = readFileSync(file, "utf8");
  for (const m of text.matchAll(T_FUNCS)) usedKeys.add(m[2]);
}

// ---- 加载四语言 ----
const localeKeys = {};
for (const lang of ["en", "zh", "ja", "zh-TW"]) {
  localeKeys[lang] = flatten(
    JSON.parse(readFileSync(join(localesDir, `${lang}.json`), "utf8")),
  );
}

// ---- 1. 漏键检查（en 为基准）----
const isDynamic = (k) => DYNAMIC_KEY_PREFIXES.some((p) => k.startsWith(p));
const missing = [...usedKeys]
  .filter((k) => !isDynamic(k) && !localeKeys.en.has(k))
  .sort();

// ---- 2. 四语言一致性 ----
const inconsistent = [];
for (const lang of ["zh", "ja", "zh-TW"]) {
  for (const k of localeKeys.en) {
    if (!localeKeys[lang].has(k))
      inconsistent.push(`en 有而 ${lang} 缺失: ${k}`);
  }
  for (const k of localeKeys[lang]) {
    if (!localeKeys.en.has(k)) inconsistent.push(`${lang} 独有: ${k}`);
  }
}

// ---- 3. 死键报告（不阻断）----
const dead = [...localeKeys.en]
  .filter((k) => !usedKeys.has(k) && !isDynamic(k))
  .sort();

let failed = false;
if (missing.length) {
  failed = true;
  console.error(`\n[X] 漏键 ${missing.length} 个（代码引用但 en.json 缺失）：`);
  for (const k of missing) console.error(`    ${k}`);
}
if (inconsistent.length) {
  failed = true;
  console.error(`\n[X] 四语言键集不一致 ${inconsistent.length} 处：`);
  for (const line of inconsistent.slice(0, 50)) console.error(`    ${line}`);
  if (inconsistent.length > 50)
    console.error(`    ... 以及另外 ${inconsistent.length - 50} 处`);
}
console.log(
  `\n[i18n] 引用键 ${usedKeys.size} | en 键 ${localeKeys.en.size} | 死键 ${dead.length}（仅报告，见 dead-i18n-keys 本地输出）`,
);
if (process.env.PRINT_DEAD_KEYS === "1" && dead.length) {
  console.log(`[i18n] 死键清单：\n  ${dead.join("\n  ")}`);
}
if (failed) {
  console.error("\n[i18n] 检查未通过 —— 键改名时必须同步迁移四语言 JSON。");
  process.exit(1);
}
console.log("[i18n] 检查通过。");
