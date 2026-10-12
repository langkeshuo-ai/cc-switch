import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";

import PromptFormPanel from "@/components/prompts/PromptFormPanel";
import type { Prompt } from "@/lib/api";

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) =>
      options?.file ? `${key}:${String(options.file)}` : key,
  }),
}));

const active: Prompt = {
  id: "team",
  name: "Team rules",
  content: "# Rules",
  enabled: true,
  createdAt: 1,
  updatedAt: 1,
};

describe("PromptFormPanel", () => {
  it("submits once and refuses to close while saving", async () => {
    let resolveSave!: () => void;
    const onSave = vi.fn(
      () =>
        new Promise<void>((resolve) => {
          resolveSave = resolve;
        }),
    );
    const onClose = vi.fn();
    render(
      <PromptFormPanel appId="claude" onSave={onSave} onClose={onClose} />,
    );

    const nameInput = screen.getByLabelText(/prompts.name/);
    fireEvent.change(nameInput, { target: { value: "My Prompt" } });
    const saveButton = screen.getByRole("button", {
      name: "prompts.addSubmit",
    });
    fireEvent.click(saveButton);
    fireEvent.click(saveButton);
    fireEvent.click(screen.getByRole("button", { name: "common.cancel" }));

    expect(onSave).toHaveBeenCalledTimes(1);
    expect(onClose).not.toHaveBeenCalled();
    expect(nameInput).toBeDisabled();
    expect(screen.getByLabelText("prompts.content")).toBeDisabled();

    await act(async () => {
      resolveSave();
      await Promise.resolve();
    });
    await waitFor(() => expect(onClose).toHaveBeenCalledTimes(1));
  });

  it("stays open when the parent write lock rejects a save", async () => {
    const onSave = vi.fn().mockResolvedValue(false);
    const onClose = vi.fn();
    render(<PromptFormPanel appId="codex" onSave={onSave} onClose={onClose} />);

    fireEvent.change(screen.getByLabelText(/prompts.name/), {
      target: { value: "Codex Prompt" },
    });
    fireEvent.click(screen.getByRole("button", { name: "prompts.addSubmit" }));

    await waitFor(() => expect(onSave).toHaveBeenCalledTimes(1));
    expect(onClose).not.toHaveBeenCalled();
    expect(screen.getByLabelText(/prompts.name/)).toBeEnabled();
  });



  it("keeps Pi content as typed and writes AGENTS.md for the enabled prompt", async () => {
    const onSave = vi.fn().mockResolvedValue(true);
    render(
      <PromptFormPanel
        appId="pi"
        editingId="team"
        initialData={active}
        onSave={onSave}
        onClose={vi.fn()}
      />,
    );

    fireEvent.change(screen.getByLabelText("prompts.content"), {
      target: { value: "  spaced\n" },
    });
    fireEvent.click(
      screen.getByRole("button", { name: "prompts.saveAndWritePi" }),
    );

    await waitFor(() =>
      expect(onSave).toHaveBeenCalledWith(
        "team",
        expect.objectContaining({ content: "  spaced\n", enabled: true }),
      ),
    );
  });

});
