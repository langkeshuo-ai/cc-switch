import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { http, HttpResponse } from "msw";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { AppRoutingPanel } from "@/components/universal/AppRoutingPanel";
import type { AppProviderDataMap } from "@/components/universal/unifiedRows";
import type { Provider } from "@/types";
import { server } from "../msw/server";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const TAURI_ENDPOINT = "http://tauri.local";

const provider = (id: string, name: string): Provider =>
  ({ id, name, settingsConfig: {} }) as Provider;

/** 面板的数据来自页面 props（与应用页同一缓存）；路由模式下 current 即路由目标（后端契约）。 */
const APP_DATA: AppProviderDataMap = {
  claude: {
    providers: {
      "claude-1": provider("claude-1", "Claude Default"),
      "claude-2": provider("claude-2", "Claude Custom"),
    },
    currentProviderId: "claude-2",
  },
  codex: {
    providers: {
      "codex-1": provider("codex-1", "Codex Default"),
      "codex-2": provider("codex-2", "Codex Secondary"),
    },
    currentProviderId: "codex-1",
  },
  pi: {
    providers: {
      "pi-1": provider("pi-1", "Pi One"),
      "pi-2": provider("pi-2", "Pi Two"),
    },
    currentProviderId: "",
  },
};

type AppModePayload = {
  mode: "direct" | "route" | "stack";
  routeProviderId: string | null;
  directProviderId: string | null;
};

let startCalls: unknown[];
let takeoverCalls: Record<string, unknown>[];

beforeEach(() => {
  Element.prototype.scrollIntoView = vi.fn();
  startCalls = [];
  takeoverCalls = [];
  server.use(
    http.post(`${TAURI_ENDPOINT}/start_proxy_server`, async () => {
      startCalls.push("start");
      return HttpResponse.json({
        address: "127.0.0.1",
        port: 15721,
        started_at: new Date().toISOString(),
      });
    }),
    http.post(`${TAURI_ENDPOINT}/set_proxy_takeover_for_app`, async ({ request }) => {
      takeoverCalls.push((await request.json()) as Record<string, unknown>);
      return HttpResponse.json(null);
    }),
  );
});

const stubAppModes = (claude: AppModePayload, codex: AppModePayload) =>
  server.use(
    http.post(`${TAURI_ENDPOINT}/get_app_mode`, async ({ request }) => {
      const { appType } = (await request.json()) as { appType: string };
      const m = appType === "claude" ? claude : codex;
      return HttpResponse.json({
        mode: m.mode,
        attached: m.mode !== "direct",
        routeProviderId: m.routeProviderId,
        directProviderId: m.directProviderId,
      });
    }),
  );

const stubProxyRunning = (running: boolean) =>
  server.use(
    http.post(`${TAURI_ENDPOINT}/get_proxy_status`, () =>
      HttpResponse.json({
        running,
        address: "127.0.0.1",
        port: 15721,
        started_at: new Date().toISOString(),
      }),
    ),
  );

const renderPanel = (onSwitch: (app: string, provider: Provider) => void) => {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  render(
    <QueryClientProvider client={queryClient}>
      <AppRoutingPanel appData={APP_DATA} onSwitch={onSwitch} />
    </QueryClientProvider>,
  );
};

const card = (name: string) =>
  within(
    screen.getByRole("heading", { name }).closest(
      "div.rounded-xl",
    ) as HTMLElement,
  );

const DIRECT_CLAUDE: AppModePayload = {
  mode: "direct",
  routeProviderId: null,
  directProviderId: "claude-1",
};
const DIRECT_CODEX: AppModePayload = {
  mode: "direct",
  routeProviderId: null,
  directProviderId: "codex-1",
};
const ROUTE_CLAUDE: AppModePayload = {
  mode: "route",
  routeProviderId: "claude-2",
  directProviderId: "claude-1",
};

describe("AppRoutingPanel", () => {
  it("renders three app cards with mode badge and effective provider", async () => {
    stubAppModes(ROUTE_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    renderPanel(vi.fn());

    const claude = card("Claude Code");
    // 模式徽标 + 子模式当前格都是「路由」
    await waitFor(() =>
      expect(claude.getAllByText("mode.names.route")).toHaveLength(2),
    );
    // 副标题与下拉当前值（隐藏 span）各出现一次
    expect(claude.getAllByText("Claude Custom")).toHaveLength(2);

    const codex = card("Codex");
    await waitFor(() =>
      expect(codex.getByText("mode.names.direct")).toBeInTheDocument(),
    );
    expect(codex.getAllByText("Codex Default")).toHaveLength(2);

    const pi = card("Pi");
    expect(
      pi.getByText("allProviders.routingPiDefault"),
    ).toBeInTheDocument();
  });

  it("turning routing on from direct opens a confirm, then enters route mode", async () => {
    stubAppModes(DIRECT_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    const user = userEvent.setup();
    renderPanel(vi.fn());

    const codexSwitch = screen.getByRole("switch", { name: /Codex/ });
    expect(codexSwitch).toHaveAttribute("aria-checked", "false");
    await user.click(codexSwitch);

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("mode.dialog.routeTitle")).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole("button", { name: "mode.dialog.confirmRoute" }),
    );

    await waitFor(() => expect(takeoverCalls).toHaveLength(1));
    expect(takeoverCalls[0]).toEqual({
      appType: "codex",
      enabled: true,
      stack: false,
      route: "codex-1",
    });
    expect(startCalls).toHaveLength(0);
  });

  it("auto-starts the proxy service before entering route mode when it is down", async () => {
    stubAppModes(DIRECT_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(false);
    const user = userEvent.setup();
    renderPanel(vi.fn());

    await user.click(screen.getByRole("switch", { name: /Codex/ }));
    const dialog = screen.getByRole("dialog");
    await user.click(
      within(dialog).getByRole("button", { name: "mode.dialog.confirmRoute" }),
    );

    await waitFor(() => expect(takeoverCalls).toHaveLength(1));
    expect(startCalls).toHaveLength(1);
    expect(takeoverCalls[0]).toMatchObject({
      appType: "codex",
      enabled: true,
      route: "codex-1",
    });
  });

  it("turning routing off exits to direct without a confirm", async () => {
    stubAppModes(ROUTE_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    const user = userEvent.setup();
    renderPanel(vi.fn());

    const claudeSwitch = screen.getByRole("switch", { name: /Claude Code/ });
    await waitFor(() =>
      expect(claudeSwitch).toHaveAttribute("aria-checked", "true"),
    );
    await user.click(claudeSwitch);

    await waitFor(() => expect(takeoverCalls).toHaveLength(1));
    expect(takeoverCalls[0]).toEqual({
      appType: "claude",
      enabled: false,
      stack: false,
      route: null,
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  });

  it("switching the sub-mode from route to stack enters stack mode", async () => {
    stubAppModes(ROUTE_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    const user = userEvent.setup();
    renderPanel(vi.fn());

    const claude = card("Claude Code");
    // 子模式格要等模式数据回来才渲染
    await waitFor(() => claude.getByRole("button", { name: "mode.names.stack" }));
    await user.click(claude.getByRole("button", { name: "mode.names.stack" }));

    const dialog = screen.getByRole("dialog");
    expect(within(dialog).getByText("mode.dialog.stackTitle")).toBeInTheDocument();
    await user.click(
      within(dialog).getByRole("button", { name: "mode.dialog.confirmStack" }),
    );

    await waitFor(() => expect(takeoverCalls).toHaveLength(1));
    expect(takeoverCalls[0]).toEqual({
      appType: "claude",
      enabled: true,
      stack: true,
      route: "claude-2",
    });
  });

  it("picking a provider in route mode routes to it (switch with app + provider)", async () => {
    stubAppModes(ROUTE_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    const user = userEvent.setup();
    const onSwitch = vi.fn();
    renderPanel(onSwitch);

    const claude = card("Claude Code");
    await user.click(claude.getByRole("combobox"));
    await user.click(screen.getByRole("option", { name: "Claude Default" }));

    expect(onSwitch).toHaveBeenCalledWith(
      "claude",
      APP_DATA.claude.providers["claude-1"],
    );
  });

  it("picking a provider in direct mode switches the direct provider", async () => {
    stubAppModes(DIRECT_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    const user = userEvent.setup();
    const onSwitch = vi.fn();
    renderPanel(onSwitch);

    const codex = card("Codex");
    await user.click(codex.getByRole("combobox"));
    await user.click(screen.getByRole("option", { name: "Codex Secondary" }));

    expect(onSwitch).toHaveBeenCalledWith(
      "codex",
      APP_DATA.codex.providers["codex-2"],
    );
  });

  it("shows a service-down notice in route mode and starts the service from it", async () => {
    stubAppModes(ROUTE_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(false);
    const user = userEvent.setup();
    renderPanel(vi.fn());

    const claude = card("Claude Code");
    await waitFor(() =>
      expect(claude.getByText("mode.notice.serviceDown")).toBeInTheDocument(),
    );
    await user.click(
      claude.getByRole("button", { name: "mode.notice.startService" }),
    );

    await waitFor(() => expect(startCalls).toHaveLength(1));
    expect(takeoverCalls).toHaveLength(0);
  });

  it("picking a pi provider sets it as the default (switch with pi + provider)", async () => {
    stubAppModes(DIRECT_CLAUDE, DIRECT_CODEX);
    stubProxyRunning(true);
    server.use(
      http.post(`${TAURI_ENDPOINT}/get_pi_current_state`, () =>
        HttpResponse.json({
          enabledProviderIds: ["pi-1", "pi-2"],
          defaultProviderId: "pi-1",
        }),
      ),
    );
    const user = userEvent.setup();
    const onSwitch = vi.fn();
    renderPanel(onSwitch);

    const pi = card("Pi");
    await user.click(pi.getByRole("combobox"));
    await user.click(screen.getByRole("option", { name: "Pi Two" }));

    expect(onSwitch).toHaveBeenCalledWith("pi", APP_DATA.pi.providers["pi-2"]);
  });
});
