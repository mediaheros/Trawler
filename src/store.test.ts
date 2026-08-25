import { beforeEach, describe, expect, it, vi } from "vitest";

import { api, type ChatRow, type Release, type SearchResponse, type ShowRow } from "./lib/api";
import { useStore } from "./store";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((ok, bad) => {
    resolve = ok;
    reject = bad;
  });
  return { promise, resolve, reject };
}

const emptySearch: SearchResponse = { releases: [], totalBeforeDedupe: 0, indexers: [] };

beforeEach(() => {
  useStore.setState({
    query: "",
    kind: "all",
    searching: false,
    hasSearched: false,
    results: null,
    searchError: null,
    shows: [],
    showsLoaded: false,
    selectedShow: null,
    status: null,
    episodeLink: null,
    grabState: {},
    toasts: [],
    chat: [],
    chatLoaded: false,
    agentBusy: false,
    agentClearing: false,
  });
});

describe("latest-only store operations", () => {
  it("does not commit a search after its query changed", async () => {
    const pending = deferred<SearchResponse>();
    vi.spyOn(api, "search").mockReturnValueOnce(pending.promise);

    useStore.getState().setQuery("old query");
    const run = useStore.getState().runSearch();
    useStore.getState().setQuery("new query");
    pending.resolve(emptySearch);
    await run;

    expect(useStore.getState().results).toBeNull();
    expect(useStore.getState().searching).toBe(false);
  });

  it("keeps the newest followed-shows snapshot when requests resolve backwards", async () => {
    const old = deferred<ShowRow[]>();
    const fresh = deferred<ShowRow[]>();
    vi.spyOn(api, "getShows")
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const oldRun = useStore.getState().loadShows();
    const freshRun = useStore.getState().loadShows();
    const newest = [{ tvmazeId: 2, name: "Newest" } as ShowRow];
    fresh.resolve(newest);
    await freshRun;
    old.resolve([{ tvmazeId: 1, name: "Stale" } as ShowRow]);
    await oldRun;

    expect(useStore.getState().shows).toEqual(newest);
  });

  it("keeps the newest connection verdict when checks resolve backwards", async () => {
    const old = deferred<Awaited<ReturnType<typeof api.testConnections>>>();
    const fresh = deferred<Awaited<ReturnType<typeof api.testConnections>>>();
    vi.spyOn(api, "testConnections")
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const oldRun = useStore.getState().refreshStatus();
    const freshRun = useStore.getState().refreshStatus();
    const green = { prowlarrOk: true, prowlarrDetail: "ok", qbitOk: true, qbitDetail: "ok" };
    fresh.resolve(green);
    await freshRun;
    old.resolve({ prowlarrOk: false, prowlarrDetail: "old", qbitOk: false, qbitDetail: "old" });
    await oldRun;

    expect(useStore.getState().status).toEqual(green);
  });

  it("never lets an older chat snapshot replace the newest completed turn", async () => {
    const old = deferred<ChatRow[]>();
    const fresh = deferred<ChatRow[]>();
    vi.spyOn(api, "agentHistory")
      .mockReturnValueOnce(old.promise)
      .mockReturnValueOnce(fresh.promise);
    const oldRun = useStore.getState().loadChat();
    const freshRun = useStore.getState().loadChat();
    const completed: ChatRow[] = [
      { id: 1, ts: 1, role: "user", content: "hello", toolName: null, toolPayload: null },
      { id: 2, ts: 2, role: "assistant", content: "done", toolName: null, toolPayload: null },
    ];
    fresh.resolve(completed);
    await freshRun;
    old.resolve([]);
    await oldRun;

    expect(useStore.getState().chat).toEqual(completed);
  });
});

it("blocks Send synchronously while conversation clear is pending", async () => {
  const clear = deferred<void>();
  vi.spyOn(api, "agentClear").mockReturnValueOnce(clear.promise);
  const send = vi.spyOn(api, "agentSend").mockResolvedValue("run");
  useStore.setState({ chatLoaded: true, agentBusy: false, agentClearing: false });

  const clearing = useStore.getState().clearChat();
  expect(useStore.getState().agentClearing).toBe(true);
  await useStore.getState().sendToAgent("must not send");
  expect(send).not.toHaveBeenCalled();
  clear.resolve(undefined);
  await clearing;
  expect(useStore.getState().agentClearing).toBe(false);
});

it("links a manual season pack to every wanted episode in that season", async () => {
  const grab = vi.spyOn(api, "grab").mockResolvedValue({ ok: true, detail: "queued" });
  vi.spyOn(api, "getShows").mockResolvedValue([]);
  useStore.setState({
    episodeLink: {
      showName: "Example",
      ep: { tvmazeEpId: 1, showId: 7, season: 2, number: 1, title: null, airstamp: null, state: "wanted", grabbedTitle: null },
      seasonWantedEpIds: [1, 2, 3],
    },
  });
  const release = {
    guid: "pack",
    title: "Example.S02.1080p",
    parsed: { season: 2, episode: null, seasonPack: true },
  } as unknown as Release;

  await useStore.getState().grab(release);

  expect(grab).toHaveBeenCalledWith(release, [1, 2, 3]);
});
