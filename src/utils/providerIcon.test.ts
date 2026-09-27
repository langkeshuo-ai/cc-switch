import { describe, expect, it } from "vitest";
import { resolveProviderIcon } from "./providerIcon";

describe("resolveProviderIcon", () => {
  it("normalizes an empty icon to the initials fallback", () => {
    expect(resolveProviderIcon("  ")).toBeUndefined();
  });

  it("returns a non-empty icon unchanged", () => {
    expect(resolveProviderIcon("grok")).toBe("grok");
  });
});
