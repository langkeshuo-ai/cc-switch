#!/usr/bin/env node
/**
 * trim-app.mjs — 把上游 tag 按声明规则裁剪成补丁（T4）。
 *
 * 用法：
 *   node scripts/trim-app.mjs <上游tag> [--out <补丁路径>]
 *
 * 行为：
 *   1. 校验 tag 存在；
 *   2. `git worktree add` 检出到临时目录；
 *   3. 按 scripts/trim-rules.json 执行删除与文本编辑：
 *      - 每一步都打印将要修改的文件（可审计）；
 *      - 编辑规则的 pattern 必须命中 minMatches 次，否则**立即中止**（失败不静默）；
 *   4. 产出 `git diff` 补丁（上游 tag → 裁剪后树）；
 *   5. 打印 manualChecklist（语义分歧点，需人工处理）。
 *
 * 幂等：同一 tag 跑两次产出的补丁逐字节一致（规则确定性 + 固定 worktree 路径）。
 * 退出码：0 成功；1 任何规则失败或 tag 不存在；2 用法错误。
 */
import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import os from "node:os";
import process from "node:process";
import { fileURLToPath } from "node:url";

const REPO_ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const RULES_PATH = path.join(REPO_ROOT, "scripts", "trim-rules.json");

function fail(msg, code = 1) {
  console.error(`✗ ${msg}`);
  process.exit(code);
}

function git(args, opts = {}) {
  return execFileSync("git", args, {
    cwd: REPO_ROOT,
    encoding: "utf8",
    maxBuffer: 64 * 1024 * 1024,
    stdio: opts.capture ? ["ignore", "pipe", "inherit"] : "inherit",
    ...(opts.extra ?? {}),
  });
}

// ---------- glob（仅支持 **、*、? 与 {a,b} 交替，够用即可，行为可预测） ----------
function globToRegExp(pattern) {
  let re = "";
  let i = 0;
  while (i < pattern.length) {
    const ch = pattern[i];
    if (ch === "*") {
      if (pattern[i + 1] === "*") {
        // `/**/` 匹配零或多级目录；`**` 贪婪跨段
        if (pattern[i + 2] === "/" && re.endsWith("/")) {
          re += "(?:[^/]+/)*";
          i += 3;
        } else {
          re += ".*";
          i += 2;
        }
      } else {
        re += "[^/]*";
        i += 1;
      }
    } else if (ch === "?") {
      re += "[^/]";
      i += 1;
    } else if (ch === "{") {
      const end = pattern.indexOf("}", i);
      if (end === -1) fail(`trim-rules.json 的 glob 花括号未闭合: ${pattern}`);
      const alts = pattern
        .slice(i + 1, end)
        .split(",")
        .map((a) => a.replace(/[.*+?^${}()|[\]\\]/g, "\\$&"));
      re += `(?:${alts.join("|")})`;
      i = end + 1;
    } else {
      re += ch.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      i += 1;
    }
  }
  return new RegExp(`^${re}$`);
}

function listFiles(root) {
  const out = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else out.push(path.relative(root, full).split(path.sep).join("/"));
    }
  };
  walk(root);
  return out.sort(); // 排序保证确定性
}

// ---------- 主流程 ----------
const args = process.argv.slice(2);
const finishMode = args.includes("--finish");
const outIdx = args.indexOf("--out");
const outIdx2 = outIdx === -1 ? Number.MAX_SAFE_INTEGER : outIdx + 1;
const tag = args.find((a, i) => !a.startsWith("-") && i !== outIdx2);
const outOverride = outIdx !== -1 ? args[outIdx + 1] : undefined;
if (!tag || (args[0] && args[0].startsWith("-") && args[0] !== "--finish")) {
  console.error("用法: node scripts/trim-app.mjs <上游tag> [--finish] [--out <补丁路径>]");
  process.exit(2);
}
if (!/^[a-zA-Z0-9._\/-]+$/.test(tag)) fail(`非法 tag: ${tag}`);

const rules = JSON.parse(fs.readFileSync(RULES_PATH, "utf8"));
console.log(`裁剪基线规则来自: ${rules.baselineUpstreamTag}，目标上游 tag: ${tag}`);

// 1. 校验 tag
try {
  git(["rev-parse", "--verify", "--quiet", `${tag}^{commit}`], { capture: true });
} catch {
  fail(`tag 不存在（先 git fetch upstream --tags）: ${tag}`);
}

// 2. worktree 检出（固定路径 → 幂等；先清理旧目录与陈旧注册）。
//    --finish 模式复用上次 prepare 留下的 worktree（其 .git 指针仍有效）。
const workdir = path.join(os.tmpdir(), `cc-switch-trim-${tag.replace(/\//g, "_")}`);
const workdirReady = finishMode && fs.existsSync(path.join(workdir, ".git"));
if (!workdirReady) {
  fs.rmSync(workdir, { recursive: true, force: true });
  try {
    git(["worktree", "prune"]); // 清掉上次异常退出留下的注册（rmSync 目录后 git 仍记得它）
    git(["worktree", "add", "--force", "--detach", "--no-checkout", workdir, tag]);
    git(["-C", workdir, "checkout", "-q", tag]);
  } catch (e) {
    fail(`worktree 检出失败: ${e.message}`);
  }
}

let actions = 0;
let hadFailure = false;
try {
  const files = workdirReady ? [] : listFiles(workdir);

  // 3a. 删除规则（--finish 复用模式跳过：上次已应用）
  for (const pattern of workdirReady ? [] : rules.deletes) {
    const re = globToRegExp(pattern);
    const matched = files.filter((f) => re.test(f));
    if (matched.length === 0) {
      // 上游删除了该文件属正常演进，不算失败，但要打印出来供审计
      console.log(`[delete] （上游已无匹配文件，跳过）${pattern}`);
      continue;
    }
    for (const f of matched) {
      fs.rmSync(path.join(workdir, f));
      console.log(`[delete] ${f}`);
      actions += 1;
    }
  }

  // 3b. 编辑规则（失败即中止；--finish 复用模式跳过）
  for (const edit of workdirReady ? [] : rules.edits) {
    const target = path.join(workdir, edit.file);
    if (!fs.existsSync(target)) {
      // 上游已重构该文件 → 人工处理，不能静默跳过
      console.error(`✗ [edit] 目标文件不存在（上游结构已变化，需人工对规则表更新）: ${edit.file} — ${edit.description}`);
      hadFailure = true;
      continue;
    }
    const content = fs.readFileSync(target, "utf8");
    const re = new RegExp(edit.pattern, "g");
    const matches = content.match(re) ?? [];
    const min = edit.minMatches ?? 1;
    if (matches.length < min) {
      console.error(
        `✗ [edit] 规则命中 ${matches.length} 次（要求 ≥${min}）: ${edit.file} — ${edit.description}`,
      );
      hadFailure = true;
      continue;
    }
    console.log(`[edit] ${edit.file} — ${edit.description}（命中 ${matches.length} 处）`);
    // 注意：占位型规则（replace 以 PLACEHOLDER_ 开头）只做命中检查，不改写——
    // 这类规则的语义必须人工处理，脚本绝不产出"看似成功实则错误"的补丁。
    if (!edit.replace.startsWith("PLACEHOLDER_")) {
      fs.writeFileSync(target, content.replace(re, edit.replace), "utf8");
      actions += 1;
    }
  }

  if (hadFailure) fail("存在未通过的编辑规则，中止（不产出补丁）。请先更新 scripts/trim-rules.json。");

  // 4. 产出补丁。
  // 不用 `git -C <workdir>`：Windows 下对临时目录内 spawn git 会被杀软/索引器
  // 稳定 EBUSY。改为显式 --git-dir/--work-tree 并以仓库根为 cwd。
  const dotGitLine = fs.readFileSync(path.join(workdir, ".git"), "utf8").trim();
  const gitdir = dotGitLine.replace(/^gitdir:\s*/i, "");
  if (!gitdir) fail("无法解析 worktree 的 .git 指针");
  let patch = "";
  for (let attempt = 1; attempt <= 8; attempt += 1) {
    try {
      patch = execFileSync(
        "git",
        ["--git-dir", gitdir, "--work-tree", workdir, "diff", "--no-color"],
        {
          cwd: REPO_ROOT,
          encoding: "utf8",
          maxBuffer: 256 * 1024 * 1024,
        },
      );
      break;
    } catch (e) {
      const busy = e.code === "EBUSY";
      if (attempt === 8) {
        if (busy) {
          // 本沙箱/杀软环境可能持续阻断 node 子进程。worktree 保留，
          // 给出确定性收尾命令（bash 下执行即可产出补丁并完成清理）。
          console.error("✗ node 子进程被环境持续阻断（EBUSY）。worktree 已保留，请用 bash 执行收尾：");
          console.error(`  git --git-dir "$(sed -n 's/^gitdir: //p' '${workdir.replace(/\\/g, "/")}/.git')" --work-tree '${workdir.replace(/\\/g, "/")}' diff --no-color > ${outOverride ?? path.join(REPO_ROOT, "cc-switch-artifacts", `trim-${tag.replace(/\//g, "_")}.patch`)}`);
          console.error(`  node scripts/trim-app.mjs ${tag} --finish   # 确认补丁产出后清理 worktree`);
        } else {
          fail(`生成补丁失败（重试 8 次后）: ${e.message}`);
        }
        process.exit(1);
      }
      console.log(`[retry] git diff 第 ${attempt} 次失败（EBUSY/瞬时锁），500ms 后重试...`);
      Atomics.wait(new Int32Array(new SharedArrayBuffer(4)), 0, 0, 500);
    }
  }
  const outPath =
    outOverride ??
    path.join(REPO_ROOT, "cc-switch-artifacts", `trim-${tag.replace(/\//g, "_")}.patch`);
  fs.mkdirSync(path.dirname(outPath), { recursive: true });
  fs.writeFileSync(outPath, patch, "utf8");
  console.log(`\n补丁已产出: ${outPath}（${patch.split("\n").length} 行，改动文件 ${actions} 处删除/编辑）`);

  // 5. 语义分歧点提醒
  console.log("\n===== manualChecklist（脚本无法自动化的语义分歧点，必须人工处理）=====");
  for (const item of rules.manualChecklist) console.log(`  - ${item}`);
  console.log("\n后续步骤：把补丁应用到 fork 分支 → 按 manualChecklist 逐项人工处理 → 跑全量闸门（typecheck + cargo check + cargo test --lib）→ 递增 -trim.N");
} finally {
  fs.rmSync(workdir, { recursive: true, force: true });
  try {
    git(["worktree", "prune"]);
  } catch {
    /* 清理失败不影响结果 */
  }
}

console.log("✓ 完成");
