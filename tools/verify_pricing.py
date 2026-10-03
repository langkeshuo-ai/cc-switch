"""校验内置模型定价表资源文件（改完 JSON 立刻自查，无需编译）。

T13 把 1192 行硬编码定价从 schema.rs 搬到
`src-tauri/src/resources/model-pricing.default.json`（沿用 codex_config.rs
既有的 resources 约定）。迁移当时的双向等价性已逐条比对证明：219 条
model_id 集合完全一致、六字段逐一相等。

硬编码已删除，故本脚本不再做"Rust vs JSON"对比，改为守护后续不变式：

  1. include_str! 指向的就是本脚本校验的这份文件（防路径漂移）
  2. JSON 合法且非空
  3. model_id 无重复（否则 INSERT OR IGNORE 静默丢行，表看着对实则少数据）
  4. 六字段全部非空（空串会进 DB，计费时静默算成 0）
  5. schema.rs 的 INSERT 列序与本脚本字段序一致（防将来改 JSON 顺序导致错位）

"JSON 条数 == DB 落盘行数"由 cargo test
`model_pricing_default_json_matches_seeded_rows` 覆盖（需编译，本脚本秒级）。

用法：python tools/verify_pricing.py    退出码非 0 即失败
"""

import io
import json
import os
import re
import sys

RUST = "src-tauri/src/database/schema.rs"
JSON = "src-tauri/src/resources/model-pricing.default.json"

FIELDS = (
    "model_id",
    "display_name",
    "input_cost_per_million",
    "output_cost_per_million",
    "cache_read_cost_per_million",
    "cache_creation_cost_per_million",
)


def fail(msg):
    print(f"[FAIL] {msg}")
    return False


def main() -> int:
    ok = True
    rust = io.open(RUST, encoding="utf-8").read()

    # 1. include_str! 路径未漂移
    m = re.search(
        r'const DEFAULT_MODEL_PRICING_JSON: &str\s*=\s*include_str!\("([^"]+)"\)', rust
    )
    if not m:
        ok = fail("未找到 DEFAULT_MODEL_PRICING_JSON 的 include_str! 定义")
    else:
        # include_str! 相对本文件所在目录 src-tauri/src/database/ 解析
        rel = m.group(1)
        # include_str! 相对本文件所在目录 src-tauri/src/database/ 解析
        resolved = os.path.normpath(
            os.path.join("src-tauri/src/database", rel)
        ).replace("\\", "/")
        if resolved != JSON:
            ok = fail(f"include_str! 指向 {resolved}，与本脚本校验的 {JSON} 不一致")
        else:
            print(f"[OK]   include_str! 路径一致 -> {JSON}")

    # 2. JSON 可解析且非空
    try:
        entries = json.load(io.open(JSON, encoding="utf-8"))
    except (OSError, ValueError) as e:
        return 1 if fail(f"读取 {JSON} 失败: {e}") else 1
    if not isinstance(entries, list) or not entries:
        return 1 if fail("JSON 不是非空数组") else 1
    print(f"[OK]   JSON 可解析，{len(entries)} 条")

    # 3. model_id 唯一
    ids = [e.get("model_id", "") for e in entries]
    dup = sorted({x for x in ids if ids.count(x) > 1})
    if dup:
        ok = fail(f"重复 model_id {dup}（会被 INSERT OR IGNORE 静默丢弃）")
    else:
        print("[OK]   model_id 无重复")

    # 4. 六字段非空
    empty = [
        (e.get("model_id", "<无 id>"), f)
        for e in entries
        for f in FIELDS
        if not str(e.get(f, "")).strip()
    ]
    if empty:
        for mid, f in empty[:5]:
            print(f"       {mid}.{f} 为空")
        ok = fail(f"共 {len(empty)} 个空字段（空串会进 DB，计费静默算成 0）")
    else:
        print("[OK]   六字段全部非空")

    # 5. INSERT 列序与本脚本字段序一致
    cols = re.search(r"INSERT OR IGNORE INTO model_pricing\s*\(([^)]*)\)", rust)
    if not cols:
        ok = fail("未找到 seed 的 INSERT 语句")
    else:
        got = [c.strip() for c in cols.group(1).split(",") if c.strip()]
        if got != list(FIELDS):
            ok = fail(f"INSERT 列序与校验字段序不一致:\n       sql={got}\n       py ={list(FIELDS)}")
        else:
            print("[OK]   INSERT 列序与校验字段序一致")

    print()
    print("注：JSON 条数 == DB 落盘行数 由 cargo test "
          "model_pricing_default_json_matches_seeded_rows 覆盖")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
