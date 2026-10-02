#!/usr/bin/env node
/**
 * i18n 键完整性护栏（防"键改名但 JSON 未迁移"事故复发，如
 * settings.advanced.snapshot.* → settings.snapshot.* 漂移）。
 *
 * 检查项：
 *   1. 漏键（阻断）：代码中 t("key") 字面量引用但 en.json 缺失。
 *   2. 四语言键集一致性（阻断）：en 有而 zh/ja/zh-TW 缺失的键。
 *   3. 死键（仅报告，不阻断）：en.json 定义但无任何引用痕迹的键。
 *
 * 引用判定（保守，宁少报不误删）——满足任一即不算死键：
 *   - t("key") / i18nKey("key") 字面量调用；
 *   - 模板字面量 t(`prefix.${expr}`) 的静态前缀覆盖；
 *   - 键的带引号形式出现在 src 或 tests 的任意源文件中（覆盖
 *     `label: "settings.x.y"` + t(preset.label) 这类变量承载的键）。
 * 命名空间本身来自变量的情形（如 t(`${ns}.empty.title`)）静态不可推断，
 * 通过 DYNAMIC_KEY_PREFIXES 白名单豁免；新增时须同步维护。
 */
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const root = join(dirname(fileURLToPath(import.meta.url)), "..");
const srcDir = join(root, "src");
const localesDir = join(root, "src", "i18n", "locales");
/**
 * 参与"引用"判定的源码根目录：src 与其同级的前端测试目录 tests。
 * 只扫 src 会把"仅被测试引用的键"误判为死键（tests/config/localeCoverage.test.ts
 * 等确实断言了若干键）。
 */
const scanRoots = [srcDir];
if (existsSync(join(root, "tests"))) scanRoots.push(join(root, "tests"));

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
  // 命名空间本身来自变量（静态不可推断）：emptyCopyNs 取值为 "pi"
  "pi.empty.",
];

/** t() 的常见别名（新增封装时同步维护） */
const T_FUNCS = /\b(?:t|i18nKey)\(\s*(['"])((?:(?!\1)[^\\])*)\1/g;

/**
 * 模板字面量键：t(`prefix.${expr}`)。
 * 取 `${` 之前的静态前缀作为动态命名空间（如 `profiles.createDescription.`），
 * 使该前缀下所有键都被视为"已引用"。无法静态推断（前缀为空，如
 * t(`${ns}.empty.title`)）的情形仍走 DYNAMIC_KEY_PREFIXES 白名单。
 */
const T_TEMPLATE = /\b(?:t|i18nKey)\(\s*`([^`]*)`/g;

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
/** 由模板字面量推断出的动态前缀（如 profiles.createDescription.） */
const templatePrefixes = new Set();
/** 源文件原文，供"带引号字面量"精确判定（不做引号配对，避免被注释里的引号带跑） */
const sourceTexts = [];
for (const dir of scanRoots) {
  for (const file of walk(dir)) {
    if (file.includes(`${"locales"}`)) continue;
    const text = readFileSync(file, "utf8");
    sourceTexts.push(text);
    for (const m of text.matchAll(T_FUNCS)) usedKeys.add(m[2]);
    for (const m of text.matchAll(T_TEMPLATE)) {
      const at = m[1].indexOf("${");
      const prefix = (at >= 0 ? m[1].slice(0, at) : m[1]).trim();
      // 无插值 → 完整字面量键；有插值 → 前缀覆盖该命名空间
      if (at < 0) usedKeys.add(prefix);
      else if (prefix) templatePrefixes.add(prefix);
    }
  }
}

/**
 * 键是否以带引号字面量出现于任意源文件。
 * 键可能不直接出现在 t() 里，而是挂在对象上经变量传入，例如
 * `label: "settings.s3Sync.presets.awsS3"` + `t(preset.label)`。
 * 这类引用静态无法解析到 t() 调用点，故采用**保守判定**：
 * 完整带引号形式出现过即视为"可能被引用"，不计入死键（宁少报，不误删）。
 */
const appearsAsQuotedLiteral = (key) =>
  sourceTexts.some(
    (text) =>
      text.includes(`"${key}"`) ||
      text.includes(`'${key}'`) ||
      text.includes("`" + key + "`"),
  );

// ---- 加载四语言 ----
const localeKeys = {};
for (const lang of ["en", "zh", "ja", "zh-TW"]) {
  localeKeys[lang] = flatten(
    JSON.parse(readFileSync(join(localesDir, `${lang}.json`), "utf8")),
  );
}

// ---- 1. 漏键检查（en 为基准）----
const isDynamic = (k) =>
  DYNAMIC_KEY_PREFIXES.some((p) => k.startsWith(p)) ||
  [...templatePrefixes].some((p) => k.startsWith(p));
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
// 保守判定：t() 字面量引用、模板前缀覆盖、或带引号字面量出现于任意源文件，
// 三者任一命中即不算死键（避免把变量承载的键误判为可删）。
const dead = [...localeKeys.en]
  .filter((k) => !usedKeys.has(k) && !appearsAsQuotedLiteral(k) && !isDynamic(k))
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
