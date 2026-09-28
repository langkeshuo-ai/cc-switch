import type { PricingModelSourceOption } from "../ProviderAdvancedConfig";

// ── Default configs ──────────────────────────────────────────────────

export const CLAUDE_DEFAULT_CONFIG = JSON.stringify({ env: {} }, null, 2);
export const CODEX_DEFAULT_CONFIG = JSON.stringify(
  { auth: {}, config: "" },
  null,
  2,
);

// ── Pure functions ───────────────────────────────────────────────────

export const normalizePricingSource = (
  value?: string,
): PricingModelSourceOption =>
  value === "request" || value === "response" ? value : "inherit";
