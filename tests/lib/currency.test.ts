import { describe, expect, it } from "vitest";
import {
  currencySymbol,
  fmtUsd,
  DEFAULT_DISPLAY_CURRENCY,
} from "@/components/usage/format";

describe("currencySymbol", () => {
  it("USD → $，CNY → ¥", () => {
    expect(currencySymbol("USD")).toBe("$");
    expect(currencySymbol("CNY")).toBe("¥");
  });
});

describe("fmtUsd 带货币符号", () => {
  it("缺省符号是 $（12 个既有调用点零破坏）", () => {
    expect(fmtUsd(1.5, 2)).toBe("$1.50");
    expect(DEFAULT_DISPLAY_CURRENCY).toBe("USD");
  });

  it("显式传 CNY 符号时渲染 ¥", () => {
    expect(fmtUsd(1.5, 2, "--", "¥")).toBe("¥1.50");
  });

  it("fallback 优先于符号渲染（无数据时不显示货币符号）", () => {
    expect(fmtUsd(null, 2, "--", "¥")).toBe("--");
    expect(fmtUsd(undefined, 2, "--", "$")).toBe("--");
  });

  it("数值与符号拼接正确，不受位数影响", () => {
    expect(fmtUsd(1234.5678, 4, "--", "¥")).toBe("¥1234.5678");
    expect(fmtUsd(0, 0, "--", "¥")).toBe("¥0");
  });
});
