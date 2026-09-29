#!/usr/bin/env bash
# 上游同步评审脚本（ADR-001 / fork-patches.md 配套）
# 用法: bash scripts/upstream-review.sh
# 输出: 落后/领先计数 + 双方共同触碰的冲突面清单 + 上游新提交按主题分类
set -euo pipefail
cd "$(dirname "$0")/.."

git fetch upstream --quiet

UP=upstream/main
MB=$(git merge-base $UP HEAD)
ahead=$(git rev-list --count $UP..HEAD)
behind=$(git rev-list --count HEAD..$UP)

echo "=== 分歧状态 ==="
echo "领先 $ahead / 落后 $behind (merge-base: ${MB:0:8})"

echo
echo "=== 上游新提交（HEAD..upstream/main）==="
git log --oneline --no-decorate HEAD..$UP

echo
echo "=== 冲突面（双方都改过的文件，共 $(comm -12 <(git diff --name-only $MB..$UP | sort) <(git diff --name-only $MB..HEAD | sort) | wc -l) 个）==="
comm -12 <(git diff --name-only $MB..$UP | sort) <(git diff --name-only $MB..HEAD | sort)

echo
echo "=== 分类建议（人工复核）==="
echo "pick 候选（fix/ 且不碰已删 app 与 key-field 引擎）:"
git log --oneline --no-decorate HEAD..$UP | grep -iE "^[0-9a-f]+ fix" | grep -viE "gemini|grok|opencode|key-field|key field" || echo "  (无)"
echo "评审候选（refactor/feat 架构级）:"
git log --oneline --no-decorate HEAD..$UP | grep -iE "^[0-9a-f]+ (refactor|feat)" | head -10 || echo "  (无)"
echo
echo "提醒: pick 一律 git cherry-pick -x；批次分支 sync/upstream-YYYYMM；闸门见 docs/fork-patches.md"
