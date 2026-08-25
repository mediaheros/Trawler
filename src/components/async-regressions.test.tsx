import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";

import { api, type Config, type HuntPlan, type TvmazeResult } from "../lib/api";
import { useStore } from "../store";
import SettingsView from "../views/Settings";
import { wrapUpdate, type NativeUpdateHandle, type UpdateInfo } from "../lib/updater";
import AddShow from "./AddShow";
import BriefsDrawer from "./BriefsDrawer";
import FirstRun from "./FirstRun";
import UpdateBanner from "./UpdateBanner";

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((ok) => {
    resolve = ok;
  });
  return { promise, resolve };
}

const plan: HuntPlan = {
  queries: ["alpha"],
  include: [],
  exclude: [],
  resolutions: ["1080p"],
  maxSizeGb: 5,
  minSeeders: 1,
  notes: "",
};

beforeEach(() => {
  useStore.setState({ briefs: [], toasts: [], status: null });
  vi.spyOn(api, "briefsList").mockResolvedValue([]);
});

it("never saves a compiled Brief plan under a different prompt", async () => {
  const user = userEvent.setup();
  const pending = deferred<HuntPlan>();
  const compile = vi.spyOn(api, "compileBrief").mockReturnValueOnce(pending.promise);
  const save = vi.spyOn(api, "briefSave").mockResolvedValue(1);
  render(<BriefsDrawer onClose={() => undefined} />);
  await user.click(screen.getByRole("button", { name: "New" }));
  const prompt = screen.getByPlaceholderText(/UFC numbered events/i);
  await user.type(prompt, "prompt A");
  await user.click(screen.getByRole("button", { name: "Compile plan" }));
  await user.clear(prompt);
  await user.type(prompt, "prompt B");
  pending.resolve(plan);

  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Create brief" }).hasAttribute("disabled")).toBe(true);
  });
  expect(save).not.toHaveBeenCalled();

  compile.mockResolvedValueOnce(plan);
  await user.click(screen.getByRole("button", { name: "Compile plan" }));
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Create brief" }).hasAttribute("disabled")).toBe(false);
  });
  await user.type(prompt, " changed");
  expect(screen.getByRole("button", { name: "Create brief" }).hasAttribute("disabled")).toBe(true);
});

it("keeps setup open and reports a failed setup completion", async () => {
  const user = userEvent.setup();
  vi.spyOn(api, "setupStatus").mockResolvedValue({
    qbit: "ok",
    prowlarr: "ok",
    agent: "unreachable",
    prowlarrHasIndexers: false,
  });
  const finish = vi.spyOn(api, "setupFinish").mockRejectedValueOnce(new Error("disk full"));
  const onDone = vi.fn();
  const view = render(<FirstRun onDone={onDone} />);

  await user.click(screen.getByRole("button", { name: /Skip/i }));
  await waitFor(() => expect(finish).toHaveBeenCalledTimes(1));
  expect(onDone).not.toHaveBeenCalled();
  const toasts = useStore.getState().toasts;
  expect(toasts[toasts.length - 1]?.text).toContain("disk full");

  finish.mockResolvedValueOnce(undefined);
  await user.click(screen.getByRole("button", { name: /Skip/i }));
  await waitFor(() => expect(onDone).toHaveBeenCalledTimes(1));
  view.unmount();
});

it("forces a fresh setup check after an action instead of reusing an older poll", async () => {
  const user = userEvent.setup();
  const before = {
    qbit: "installed_stopped" as const,
    prowlarr: "ok" as const,
    agent: "unreachable" as const,
    prowlarrHasIndexers: true,
  };
  const stalePoll = deferred<typeof before>();
  const ready = { ...before, qbit: "ok" as const };
  const freshPoll = deferred<typeof ready>();
  const status = vi
    .spyOn(api, "setupStatus")
    .mockResolvedValueOnce(before)
    .mockReturnValueOnce(stalePoll.promise)
    .mockReturnValueOnce(freshPoll.promise);
  const configure = vi.spyOn(api, "setupConfigureQbit").mockResolvedValue(undefined);
  const view = render(<FirstRun onDone={() => undefined} />);

  await waitFor(() => expect(screen.getByRole("button", { name: /Enable Web UI/i })).toBeTruthy());
  await user.click(screen.getByRole("button", { name: /re-check now/i }));
  await waitFor(() => expect(status).toHaveBeenCalledTimes(2));
  await user.click(screen.getByRole("button", { name: /Enable Web UI/i }));
  await waitFor(() => expect(configure).toHaveBeenCalledTimes(1));

  stalePoll.resolve(before);

  await waitFor(() => expect(status).toHaveBeenCalledTimes(3));
  const action = screen.getByRole("button", { name: /Enable Web UI/i });
  expect(action.hasAttribute("disabled")).toBe(true);
  await user.click(action);
  expect(configure).toHaveBeenCalledTimes(1);

  freshPoll.resolve(ready);
  await waitFor(() => {
    expect(screen.getByRole("button", { name: "Start hunting" }).hasAttribute("disabled")).toBe(false);
  });
  view.unmount();
});

it("preserves settings typed while an older save is pending", async () => {
  const user = userEvent.setup();
  const original = await api.getConfig();
  useStore.setState({ config: original, toasts: [] });
  const pending = deferred<Config>();
  const save = vi.spyOn(api, "setConfig").mockReturnValueOnce(pending.promise);
  vi.spyOn(api, "testConnections").mockResolvedValue({
    prowlarrOk: true,
    prowlarrDetail: "ok",
    qbitOk: true,
    qbitDetail: "ok",
  });
  render(<SettingsView />);
  const input = screen.getByDisplayValue(original.prowlarrUrl) as HTMLInputElement;
  await user.clear(input);
  await user.type(input, "http://saved.example:9696");
  await user.click(screen.getByRole("button", { name: "Save changes" }));
  await waitFor(() => expect(save).toHaveBeenCalledTimes(1));
  const submitted = save.mock.calls[0][0];

  await user.clear(input);
  await user.type(input, "http://newer.example:9696");
  pending.resolve(submitted);

  await waitFor(() => expect(useStore.getState().config?.prowlarrUrl).toBe("http://saved.example:9696"));
  expect(input.value).toBe("http://newer.example:9696");
  expect(screen.getByText("Unsaved changes")).toBeTruthy();
});

it("cannot install a replacement update handle using an older download", async () => {
  const user = userEvent.setup();
  const pending = deferred<void>();
  const oldUpdate: UpdateInfo = {
    version: "1.0.0",
    download: vi.fn(() => pending.promise),
    install: vi.fn(),
    close: vi.fn(async () => undefined),
  };
  const replacement: UpdateInfo = {
    version: "1.0.1",
    download: vi.fn(async () => undefined),
    install: vi.fn(),
    close: vi.fn(async () => undefined),
  };
  useStore.setState({ pendingUpdate: oldUpdate, updateDismissedVersion: null });
  render(<UpdateBanner />);
  await user.click(screen.getByRole("button", { name: "Update now" }));
  useStore.getState().setPendingUpdate(replacement);
  pending.resolve(undefined);

  await waitFor(() => expect(screen.getByRole("button", { name: "Update now" })).toBeTruthy());
  expect(screen.queryByRole("button", { name: /Install & restart/i })).toBeNull();
  expect(replacement.install).not.toHaveBeenCalled();
});

it("closes updater bytes created after a replaced download finishes", async () => {
  const pending = deferred<void>();
  let downloadedBytesExist = false;
  const native: NativeUpdateHandle = {
    version: "1.0.0",
    download: vi.fn(async () => {
      await pending.promise;
      downloadedBytesExist = true;
    }),
    install: vi.fn(),
    close: vi.fn(async () => {
      expect(downloadedBytesExist).toBe(true);
      downloadedBytesExist = false;
    }),
  };
  const update = wrapUpdate(native);
  const download = update.download(() => undefined);
  const close = update.close();
  expect(native.close).not.toHaveBeenCalled();
  pending.resolve(undefined);
  await Promise.all([download, close]);
  expect(native.close).toHaveBeenCalledTimes(1);
  expect(downloadedBytesExist).toBe(false);
  await update.close();
  expect(native.close).toHaveBeenCalledTimes(1);
});

const tvResults: TvmazeResult[] = [
  { id: 1, name: "Alpha", premiered: null, ended: null, network: null, status: "Running", poster: null, summary: null, genres: [], followed: false },
  { id: 2, name: "Beta", premiered: null, ended: null, network: null, status: "Running", poster: null, summary: null, genres: [], followed: false },
];

it("does not repopulate Add Show after a pending query is cleared", async () => {
  const user = userEvent.setup();
  const pending = deferred<typeof tvResults>();
  const search = vi.spyOn(api, "searchTvmaze").mockReturnValueOnce(pending.promise);
  render(<AddShow onClose={() => undefined} />);
  const input = screen.getByPlaceholderText(/Search TV shows/i);
  await user.type(input, "alpha");
  await waitFor(() => expect(search).toHaveBeenCalledTimes(1), { timeout: 1200 });
  await user.clear(input);
  pending.resolve(tvResults);

  await waitFor(() => expect(screen.getByText("Type a show name to search TVmaze.")).toBeTruthy());
  expect(screen.queryByText("Alpha")).toBeNull();
});

it("allows only one follow operation at a time", async () => {
  const user = userEvent.setup();
  vi.spyOn(api, "searchTvmaze").mockResolvedValue(tvResults);
  const pending = deferred<void>();
  const follow = vi.fn(() => pending.promise);
  useStore.setState({ shows: [], followShow: follow });
  render(<AddShow onClose={() => undefined} />);
  await user.type(screen.getByPlaceholderText(/Search TV shows/i), "show");
  await waitFor(() => expect(screen.getAllByRole("button", { name: "Follow" })).toHaveLength(2), {
    timeout: 1200,
  });
  const buttons = screen.getAllByRole("button", { name: "Follow" });
  await user.click(buttons[0]);
  await user.click(buttons[1]);
  expect(follow).toHaveBeenCalledTimes(1);
  pending.resolve(undefined);
});
