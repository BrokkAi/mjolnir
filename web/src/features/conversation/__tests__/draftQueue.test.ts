import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { putDraft } from "../../../api/client";
import { useSessionStore } from "../../../stores/useSessionStore";
import {
  DRAFT_SAVE_DELAY_MS,
  clearAllDraftState,
  getPendingDraft,
  scheduleDraftSave,
  writeDraftNow,
} from "../draftQueue";

vi.mock("../../../api/client", () => ({
  putDraft: vi.fn(),
  configureApi: vi.fn(),
}));

const flushMicrotasks = () => Promise.resolve().then(() => Promise.resolve());

describe("draft state across sign-out", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.mocked(putDraft).mockReset();
    clearAllDraftState();
  });
  afterEach(() => {
    clearAllDraftState();
    vi.useRealTimers();
  });

  it("cancels a pending save timer so the next viewer sees no leaked text", () => {
    vi.mocked(putDraft).mockResolvedValue(undefined);
    scheduleDraftSave("s1", "private words");
    expect(getPendingDraft("s1")).toBe("private words");

    useSessionStore.getState().handleSignedOut();
    vi.advanceTimersByTime(DRAFT_SAVE_DELAY_MS * 2);

    expect(putDraft).not.toHaveBeenCalled();
    expect(getPendingDraft("s1")).toBeNull();
  });

  it("drops the completion of a write in flight: no follow-up write and no recorded save", async () => {
    let finish: () => void = () => {};
    vi.mocked(putDraft).mockImplementation(
      () => new Promise<void>((resolve) => (finish = resolve)),
    );
    writeDraftNow("s1", "one");
    expect(putDraft).toHaveBeenCalledTimes(1);
    scheduleDraftSave("s1", "two"); // newer text waiting behind the write in flight

    useSessionStore.getState().handleSignedOut();
    finish();
    await flushMicrotasks();
    vi.advanceTimersByTime(DRAFT_SAVE_DELAY_MS * 2);
    await flushMicrotasks();

    expect(putDraft).toHaveBeenCalledTimes(1); // "two" never goes out
    expect(getPendingDraft("s1")).toBeNull();
    // The old write was not recorded as saved for the next viewer: "one" still needs a write.
    vi.mocked(putDraft).mockResolvedValue(undefined);
    scheduleDraftSave("s1", "one");
    expect(getPendingDraft("s1")).toBe("one");
  });
});
