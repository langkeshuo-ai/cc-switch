import { describe, expect, it } from "vitest";
import {
  createKeyPoolRow,
  normalizeKeyPool,
  type KeyPoolRow,
} from "@/components/providers/forms/KeyPoolField";

describe("normalizeKeyPool", () => {
  it("去掉空行（key 为空的行不进池）", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "a", key: "sk-a" }),
      createKeyPoolRow({ id: "b", key: "" }),
      createKeyPoolRow({ id: "c", key: "   " }),
      createKeyPoolRow({ id: "d", key: "sk-d" }),
    ];
    expect(normalizeKeyPool(rows)).toEqual([
      { id: "a", key: "sk-a" },
      { id: "d", key: "sk-d" },
    ]);
  });

  it("没填 id 时按行序补稳定标识", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ key: "sk-a" }),
      createKeyPoolRow({ key: "sk-b" }),
    ];
    const result = normalizeKeyPool(rows);
    expect(result.map((entry) => entry.id)).toEqual(["key-1", "key-2"]);
  });

  it("保留用户显式填的 id（编辑存量池时不改变归属键）", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "my-custom-id", key: "sk-a" }),
      createKeyPoolRow({ key: "sk-b" }),
    ];
    expect(normalizeKeyPool(rows).map((entry) => entry.id)).toEqual([
      "my-custom-id",
      "key-2",
    ]);
  });

  it("空 label 被剥掉（不写 undefined 字段）", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "a", key: "sk-a", label: "   " }),
      createKeyPoolRow({ id: "b", key: "sk-b", label: "备用" }),
    ];
    const result = normalizeKeyPool(rows);
    expect(result[0]).toEqual({ id: "a", key: "sk-a" });
    expect("label" in result[0]).toBe(false);
    expect(result[1]).toEqual({ id: "b", key: "sk-b", label: "备用" });
  });

  it("key 前后空白被去掉", () => {
    const rows: KeyPoolRow[] = [createKeyPoolRow({ id: "a", key: "  sk-a  " })];
    expect(normalizeKeyPool(rows)).toEqual([{ id: "a", key: "sk-a" }]);
  });

  it("空数组进、空数组出", () => {
    expect(normalizeKeyPool([])).toEqual([]);
  });

  it("N2：日限额保留、空白剥除、留空剥除字段", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "a", key: "sk-a", dailyLimitUsd: " 5 " }),
      createKeyPoolRow({ id: "b", key: "sk-b", dailyLimitUsd: "   " }),
    ];
    const result = normalizeKeyPool(rows);
    expect(result[0]).toEqual({ id: "a", key: "sk-a", dailyLimitUsd: "5" });
    expect("dailyLimitUsd" in result[1]).toBe(false);
  });

  it("R7/R10：并发与 RPM 上限保留、非法值剥除", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "a", key: "sk-a", maxConcurrency: 3, maxRpm: 20 }),
      createKeyPoolRow({ id: "b", key: "sk-b", maxConcurrency: 0, maxRpm: -5 }),
    ];
    const result = normalizeKeyPool(rows);
    expect(result[0]).toEqual({
      id: "a",
      key: "sk-a",
      maxConcurrency: 3,
      maxRpm: 20,
    });
    expect("maxConcurrency" in result[1]).toBe(false);
    expect("maxRpm" in result[1]).toBe(false);
  });

  it("序列化往返：normalize 的产物可直接作为 meta.keyPool", () => {
    const rows: KeyPoolRow[] = [
      createKeyPoolRow({ id: "a", key: "sk-a", label: "主 key" }),
      createKeyPoolRow({ key: "sk-b" }),
    ];
    const pool = normalizeKeyPool(rows);
    // 与 Rust 侧 serde 契约一致：{id, key, label?}
    const roundTripped = JSON.parse(JSON.stringify({ keyPool: pool })).keyPool;
    expect(roundTripped).toEqual(pool);
    // 每个 entry 必须有 id 与 key（id 是冷却与 usage 归属的键）
    for (const entry of pool) {
      expect(entry.id).toBeTruthy();
      expect(entry.key).toBeTruthy();
    }
  });
});
