// @vitest-environment jsdom
/**
 * `useFilters<T>()` sessionStorage persistence — the fix for "changing
 * a filter, then hitting refresh (or navigating to another dashboard
 * page and back), loses the filter." See hooks/use-filters.ts's own
 * design-notes doc for the full rationale (sessionStorage over
 * localStorage, per-consumer storageKey, shape-drift tolerance).
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest"
import { act, renderHook } from "@testing-library/react"
import { useFilters } from "@/hooks/use-filters"

beforeEach(() => {
  window.sessionStorage.clear()
})
afterEach(() => {
  window.sessionStorage.clear()
})

describe("useFilters<T>() sessionStorage persistence", () => {
  it("without storageKey, behaves exactly as before (no persistence, no crash)", () => {
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({ initial: { q: "" } }),
    )
    act(() => result.current.setFilter("q", "hello"))
    expect(result.current.filters.q).toBe("hello")
    expect(window.sessionStorage.length).toBe(0)
  })

  it("persists a filter change to sessionStorage under storageKey", () => {
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.a",
      }),
    )
    act(() => result.current.setFilter("q", "hello"))
    expect(
      JSON.parse(window.sessionStorage.getItem("test.filters.a")!),
    ).toEqual({ q: "hello" })
  })

  it("restores a persisted filter on a fresh mount (simulates a page refresh)", () => {
    window.sessionStorage.setItem(
      "test.filters.b",
      JSON.stringify({ q: "restored" }),
    )
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.b",
      }),
    )
    // Restored BEFORE first paint (lazy useState initializer) -- no
    // flash of the default filters.
    expect(result.current.filters.q).toBe("restored")
  })

  it("restores a persisted filter across an unmount+remount (simulates navigating away and back)", () => {
    const { result, unmount } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.c",
      }),
    )
    act(() => result.current.setFilter("q", "kept"))
    unmount()

    const { result: result2 } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.c",
      }),
    )
    expect(result2.current.filters.q).toBe("kept")
  })

  it("clearAll resets AND persists the reset (a stale filter doesn't come back on the next mount)", () => {
    const { result, unmount } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.d",
      }),
    )
    act(() => result.current.setFilter("q", "narrow"))
    act(() => result.current.clearAll())
    unmount()

    const { result: result2 } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.d",
      }),
    )
    expect(result2.current.filters.q).toBe("")
  })

  it("two different storageKeys never cross-contaminate", () => {
    const { result: a } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.e1",
      }),
    )
    const { result: b } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.e2",
      }),
    )
    act(() => a.current.setFilter("q", "for-a"))
    expect(b.current.filters.q).toBe("")
  })

  it("tolerates a persisted blob with a field the current build no longer declares (shape drift)", () => {
    window.sessionStorage.setItem(
      "test.filters.f",
      JSON.stringify({ q: "kept", legacyField: "should be dropped" }),
    )
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.f",
      }),
    )
    expect(result.current.filters).toEqual({ q: "kept" })
  })

  it("tolerates a persisted blob missing a field the current build added (falls back to initial for that field)", () => {
    window.sessionStorage.setItem(
      "test.filters.g",
      JSON.stringify({ q: "kept" }),
    )
    const { result } = renderHook(() =>
      useFilters<{ q: string; newField: string }>({
        initial: { q: "", newField: "default" },
        storageKey: "test.filters.g",
      }),
    )
    expect(result.current.filters).toEqual({ q: "kept", newField: "default" })
  })

  it("tolerates corrupt JSON in storage -- falls back to initial, never throws", () => {
    window.sessionStorage.setItem("test.filters.h", "{not valid json")
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.h",
      }),
    )
    expect(result.current.filters).toEqual({ q: "" })
  })

  it("onReset still fires on a persisted setFilter change, same as without persistence", () => {
    let resetCount = 0
    const { result } = renderHook(() =>
      useFilters<{ q: string }>({
        initial: { q: "" },
        storageKey: "test.filters.i",
        onReset: () => {
          resetCount += 1
        },
      }),
    )
    act(() => result.current.setFilter("q", "x"))
    expect(resetCount).toBe(1)
  })
})
