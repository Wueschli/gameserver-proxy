import { act, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ApiError } from "../api";

const api = vi.hoisted(() => ({
  listPlugins: vi.fn(),
  uploadPluginModule: vi.fn(),
  installPlugin: vi.fn(),
  setPluginEnabled: vi.fn(),
  deletePlugin: vi.fn(),
}));
vi.mock("../api", async (orig) => ({ ...(await orig<typeof import("../api")>()), ...api }));

import { PluginsPage } from "./PluginsPage";

const NOW = 1_800_000_000;
const CAPS = {
  triggers: { on_timer: true },
  tick_interval_secs: 60,
  log: true,
  state: { max_bytes: 65536 },
};
const SHA = "ab".repeat(32);

function install(extra = {}) {
  return {
    id: "0123456789abcdef",
    name: "pelican-sync",
    sha256: SHA,
    size: 2048,
    approved: CAPS,
    config: {},
    enabled: true,
    created_at: NOW - 3600,
    created_by: "alice",
    ...extra,
  };
}

function wasm(name = "demo.wasm") {
  return new File([new Uint8Array([0, 97, 115, 109])], name, { type: "application/wasm" });
}

beforeEach(() => {
  vi.spyOn(Date, "now").mockReturnValue(NOW * 1000);
  api.listPlugins.mockReset().mockResolvedValue([install()]);
  api.uploadPluginModule
    .mockReset()
    .mockResolvedValue({ sha256: SHA, size: 2048, abi: "0.1", capabilities: CAPS });
  api.installPlugin.mockReset().mockResolvedValue(install({ name: "demo" }));
  api.setPluginEnabled.mockReset().mockResolvedValue(install({ enabled: false }));
  api.deletePlugin.mockReset().mockResolvedValue(undefined);
});

describe("PluginsPage list", () => {
  it("shows each install with its approved capabilities in words", async () => {
    render(<PluginsPage />);
    const row = (await screen.findByText("pelican-sync")).closest("tr")!;
    expect(within(row).getByText("enabled")).toBeInTheDocument();
    expect(within(row).getByText(/runs every 60 s/i)).toBeInTheDocument();
    expect(within(row).getByText(/writes log lines/i)).toBeInTheDocument();
    expect(within(row).getByText(/keeps up to 64 KiB of state/i)).toBeInTheDocument();
    expect(within(row).getByText("abababababab…")).toBeInTheDocument();
  });

  it("shows the controller's reason when plugins are not served", async () => {
    api.listPlugins.mockRejectedValue(
      new ApiError(501, "plugins are off; start the controller with --plugins"),
    );
    render(<PluginsPage />);
    expect(await screen.findByText(/start the controller with --plugins/)).toBeInTheDocument();
    expect(screen.queryByRole("table")).not.toBeInTheDocument();
  });

  it("says so when there are no installs", async () => {
    api.listPlugins.mockResolvedValue([]);
    render(<PluginsPage />);
    expect(await screen.findByText(/No plugins installed/)).toBeInTheDocument();
  });
});

describe("PluginsPage enable, disable and delete", () => {
  it("disables an enabled plugin and enables a disabled one", async () => {
    api.listPlugins.mockResolvedValueOnce([install()]).mockResolvedValue([install({ enabled: false })]);
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Disable pelican-sync" }));
    expect(api.setPluginEnabled).not.toHaveBeenCalled();
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Disable" }));
    expect(api.setPluginEnabled).toHaveBeenCalledWith("0123456789abcdef", false);

    await userEvent.click(await screen.findByRole("button", { name: "Enable pelican-sync" }));
    expect(api.setPluginEnabled).toHaveBeenLastCalledWith("0123456789abcdef", true);
  });

  it("does not disable when the confirmation is cancelled", async () => {
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Disable pelican-sync" }));
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Cancel" }));
    expect(api.setPluginEnabled).not.toHaveBeenCalled();
  });

  it("asks before deleting and only then deletes", async () => {
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Delete pelican-sync" }));
    expect(api.deletePlugin).not.toHaveBeenCalled();
    const dlg = screen.getByRole("dialog");
    expect(dlg).toHaveTextContent("pelican-sync");
    await userEvent.click(within(dlg).getByRole("button", { name: "Delete" }));
    expect(api.deletePlugin).toHaveBeenCalledWith("0123456789abcdef");
  });

  it("shows a failed action's message", async () => {
    api.setPluginEnabled.mockRejectedValue(new ApiError(404, "no such install"));
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Disable pelican-sync" }));
    await userEvent.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Disable" }));
    expect(await screen.findByText(/no such install/)).toBeInTheDocument();
  });
});

describe("PluginsPage install", () => {
  async function upload(file = wasm()) {
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Upload plugin" }));
    const dlg = screen.getByRole("dialog");
    await userEvent.upload(within(dlg).getByLabelText(/\.wasm file/), file);
    await userEvent.click(within(dlg).getByRole("button", { name: "Inspect module" }));
    return await screen.findByRole("dialog");
  }

  it("shows what the module declares and keeps Install disabled until approved", async () => {
    const dlg = await upload();
    expect(api.uploadPluginModule).toHaveBeenCalledTimes(1);
    expect(dlg).toHaveTextContent(/runs every 60 s/i);
    expect(dlg).toHaveTextContent(/cannot make network requests/i);
    expect(dlg).toHaveTextContent(/uploaded by hand/i);
    const button = within(dlg).getByRole("button", { name: "Install" });
    expect(button).toBeDisabled();
    await userEvent.click(within(dlg).getByRole("checkbox", { name: /I approve/i }));
    expect(button).toBeEnabled();
  });

  it("installs with exactly the declared capabilities and the file's name", async () => {
    const dlg = await upload(wasm("Pelican_Sync.wasm"));
    expect(within(dlg).getByLabelText(/^Name/)).toHaveValue("pelican-sync");
    await userEvent.click(within(dlg).getByRole("checkbox", { name: /I approve/i }));
    await userEvent.click(within(dlg).getByRole("button", { name: "Install" }));
    expect(api.installPlugin).toHaveBeenCalledWith({
      name: "pelican-sync",
      sha256: SHA,
      approved: CAPS,
      enabled: true,
    });
  });

  it("shows the controller's refusal and keeps the dialog open", async () => {
    api.installPlugin.mockRejectedValue(new ApiError(422, "capability not approved: log"));
    const dlg = await upload();
    await userEvent.click(within(dlg).getByRole("checkbox", { name: /I approve/i }));
    await userEvent.click(within(dlg).getByRole("button", { name: "Install" }));
    expect(await screen.findByText(/capability not approved: log/)).toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeInTheDocument();
  });

  it("lists capabilities it has no wording for and drops the 'cannot' sentence", async () => {
    api.uploadPluginModule.mockResolvedValue({
      sha256: SHA,
      size: 2048,
      abi: "0.1",
      capabilities: { ...CAPS, http: { hosts: ["panel.example"] } },
    });
    const dlg = await upload();
    expect(dlg).toHaveTextContent("Also asks for: http");
    expect(dlg).not.toHaveTextContent(/cannot make network requests/i);
  });

  it("refuses a file over 8 MiB without uploading it", async () => {
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Upload plugin" }));
    const dlg = screen.getByRole("dialog");
    const big = new File([new Uint8Array(1)], "big.wasm");
    Object.defineProperty(big, "size", { value: 8 * 1024 * 1024 + 1 });
    await userEvent.upload(within(dlg).getByLabelText(/\.wasm file/), big);
    await userEvent.click(within(dlg).getByRole("button", { name: "Inspect module" }));
    expect(await screen.findByText(/limited to 8 MiB/)).toBeInTheDocument();
    expect(api.uploadPluginModule).not.toHaveBeenCalled();
  });

  it("ignores the answer to an upload that was cancelled", async () => {
    let finish!: (m: unknown) => void;
    api.uploadPluginModule.mockReturnValue(new Promise((resolve) => (finish = resolve)));
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Upload plugin" }));
    let dlg = screen.getByRole("dialog");
    await userEvent.upload(within(dlg).getByLabelText(/\.wasm file/), wasm());
    await userEvent.click(within(dlg).getByRole("button", { name: "Inspect module" }));
    await userEvent.click(within(dlg).getByRole("button", { name: "Cancel" }));
    await act(async () => finish({ sha256: SHA, size: 2048, abi: "0.1", capabilities: CAPS }));

    await userEvent.click(screen.getByRole("button", { name: "Upload plugin" }));
    dlg = screen.getByRole("dialog");
    expect(within(dlg).getByRole("button", { name: "Inspect module" })).toBeInTheDocument();
    expect(within(dlg).queryByRole("button", { name: "Install" })).not.toBeInTheDocument();
  });

  it("still reports an install whose dialog was closed meanwhile, without disturbing a reopened dialog", async () => {
    let finish!: (v: unknown) => void;
    api.installPlugin.mockReturnValue(new Promise((resolve) => (finish = resolve)));
    const dlg = await upload();
    await userEvent.click(within(dlg).getByRole("checkbox", { name: /I approve/i }));
    await userEvent.click(within(dlg).getByRole("button", { name: "Install" }));
    await userEvent.click(within(dlg).getByRole("button", { name: "Cancel" }));
    await userEvent.click(screen.getByRole("button", { name: "Upload plugin" }));
    await act(async () => finish(install({ name: "demo" })));
    expect(await screen.findByText("installed demo")).toBeInTheDocument();
    expect(within(screen.getByRole("dialog")).getByRole("button", { name: "Inspect module" })).toBeEnabled();
  });

  it("shows an upload error from the controller", async () => {
    api.uploadPluginModule.mockRejectedValue(new ApiError(400, "not a plugin module"));
    render(<PluginsPage />);
    await userEvent.click(await screen.findByRole("button", { name: "Upload plugin" }));
    const dlg = screen.getByRole("dialog");
    await userEvent.upload(within(dlg).getByLabelText(/\.wasm file/), wasm());
    await userEvent.click(within(dlg).getByRole("button", { name: "Inspect module" }));
    expect(await screen.findByText(/not a plugin module/)).toBeInTheDocument();
  });
});
