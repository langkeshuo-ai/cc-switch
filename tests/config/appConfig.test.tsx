import { describe, expect, it } from "vitest";
import { isAdditiveAppId } from "@/config/appConfig";

describe("appConfig provider lifecycle", () => {
  it.each(["pi"])("classifies %s as additive", (appId) => {
    expect(isAdditiveAppId(appId)).toBe(true);
  });

  it.each(["claude", "codex"])("does not classify %s as additive", (appId) => {
    expect(isAdditiveAppId(appId)).toBe(false);
  });
});
