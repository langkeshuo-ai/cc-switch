import type { ReactNode } from "react";
import { render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import CodexConfigEditor from "@/components/providers/forms/CodexConfigEditor";

vi.mock("@/components/common/FullScreenPanel", () => ({
  FullScreenPanel: ({
    isOpen,
    title,
    onClose,
    children,
    footer,
  }: {
    isOpen: boolean;
    title: string;
    onClose: () => void;
    children: ReactNode;
    footer?: ReactNode;
  }) =>
    isOpen ? (
      <div data-testid="common-config-panel">
        <button type="button" onClick={onClose}>
          panel-close
        </button>
        <h2>{title}</h2>
        <div>{children}</div>
        <div>{footer}</div>
      </div>
    ) : null,
}));

vi.mock("@/components/JsonEditor", () => ({
  default: ({
    value,
    onChange,
  }: {
    value: string;
    onChange: (value: string) => void;
  }) => (
    <textarea
      value={value}
      onChange={(event) => onChange(event.target.value)}
      aria-label="mock-editor"
    />
  ),
}));

describe("Common config modals", () => {
  it("shows no Codex common config snippet and lists fields that do not follow the provider", () => {
    render(
      <CodexConfigEditor
        authValue="{}"
        configValue=""
        onAuthChange={() => {}}
        onConfigChange={() => {}}
        authError=""
        configError=""
        inactiveFields={[
          {
            path: ["mcp_servers", "legacy"],
            value: '[mcp_servers.legacy]\ncommand = "x"\n',
          },
        ]}
      />,
    );

    expect(
      screen.queryByRole("button", {
        name: /codexConfig.editCommonConfig|编辑通用配置/,
      }),
    ).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "mcp_servers.legacy" }),
    ).toBeInTheDocument();
  });

});
