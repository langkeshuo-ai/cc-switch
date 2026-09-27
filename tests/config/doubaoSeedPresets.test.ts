import { describe, expect, it } from "vitest";
import { codexProviderPresets } from "@/config/codexProviderPresets";

describe("Volcengine Doubao preset", () => {
  const DOUBAO_MODEL_ID = "doubao-seed-2-1-pro-260628";
  const EXPECTED_CONTEXT_WINDOW = 262144;

  it("keeps the doubao context window in the Codex catalog", () => {
    const codexPreset = codexProviderPresets.find(
      (item) => item.name === "Volcengine Doubao",
    );
    const codexModel = (codexPreset?.modelCatalog ?? []).find(
      (model) => model.model === DOUBAO_MODEL_ID,
    );
    expect(codexModel, "Codex Volcengine Doubao catalog model").toBeDefined();
    expect(codexModel?.contextWindow).toBe(EXPECTED_CONTEXT_WINDOW);
  });
});
