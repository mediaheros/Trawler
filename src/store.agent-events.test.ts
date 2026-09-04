import { expect, it, vi } from "vitest";

import * as apiModule from "./lib/api";
import { useStore } from "./store";

// its own file: initAgentEvents is once-per-module, so the captured listener
// must belong to this test alone

it("reloads the transcript when a chat run ends in an error", async () => {
  let listener: ((s: apiModule.AgentStep) => void) | null = null;
  vi.spyOn(apiModule, "onAgentStep").mockImplementation((cb) => {
    listener = cb;
    return () => undefined;
  });
  const history = vi.spyOn(apiModule.api, "agentHistory").mockResolvedValue([
    { id: 1, ts: 1, role: "user", content: "hi", toolName: null, toolPayload: null },
    { id: 2, ts: 2, role: "assistant", content: "⚠ the run exceeded its 6-minute deadline", toolName: null, toolPayload: null },
  ]);
  useStore.setState({
    chat: [{ id: -1, ts: 1, role: "user", content: "hi", toolName: null, toolPayload: null }],
    chatLoaded: true,
    agentBusy: true,
    agentThinking: true,
    liveSteps: [{ key: 1, tool: "search_releases", summary: "Searching…", running: true }],
    toasts: [],
  });
  useStore.getState().initAgentEvents();
  expect(listener).not.toBeNull();

  listener!({ runId: "chat-1", kind: "error", payload: { message: "boom" } });

  expect(useStore.getState().agentBusy).toBe(false);
  expect(useStore.getState().liveSteps.every((s) => !s.running)).toBe(true);
  // the backend persisted a "⚠ …" assistant row; the transcript must show it
  // now, not after the next view switch
  await vi.waitFor(() => expect(history).toHaveBeenCalled());
  await vi.waitFor(() => {
    const chat = useStore.getState().chat;
    const last = chat[chat.length - 1];
    expect(last?.role).toBe("assistant");
    expect(last?.content).toContain("⚠");
  });
});
