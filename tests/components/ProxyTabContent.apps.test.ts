import { describe, expect, it } from "vitest";
import { FAILOVER_APP_IDS, PROXY_APP_IDS } from "@/config/appConfig";
import { FAILOVER_APPS } from "@/components/settings/ProxyTabContent";

describe("ProxyTabContent failover apps", () => {
  // 断言必须与 appConfig.tsx 的 FailoverAppId 契约一致：failover 比 PROXY_APP_IDS
  // 窄，**不含 pi**。Pi 的透传无法安全切换方言，后端 `commands/failover.rs` 直接拒绝，
  // 前端若把它列为 failover 可用项就是在宣传后端拒绝的能力。
  // 原断言 ["claude","codex","pi"] 与该契约脱节，属测试落后于实现。
  it("excludes pi, whose forwarding cannot switch dialects safely", () => {
    expect(FAILOVER_APPS.map(({ id }) => id)).toEqual(["claude", "codex"]);
  });

  it("stays a strict subset of the proxy-capable apps", () => {
    const failover = FAILOVER_APP_IDS as readonly string[];
    const proxy = PROXY_APP_IDS as readonly string[];

    for (const id of failover) {
      expect(proxy).toContain(id);
    }
    // 严格子集：若某天两者相等，说明 failover 覆盖面变了，本测试应同步复核
    expect(failover.length).toBeLessThan(proxy.length);
  });
});
