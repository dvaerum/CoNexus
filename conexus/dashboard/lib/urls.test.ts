import { describe, expect, it } from "vitest"

import { deriveMount } from "./urls"

// ADR-0020: the dashboard derives its mount prefix from the URL at
// runtime so ONE build serves both the tailnet (/conexus/…) and a
// Traefik root front door (/…). The critical tailnet-regression guard is
// that a /conexus/… path still derives exactly "/conexus".
describe("deriveMount", () => {
  it("returns /conexus for tailnet app paths (byte-identical)", () => {
    expect(deriveMount("/conexus/app/foo/")).toBe("/conexus")
    expect(deriveMount("/conexus/app/")).toBe("/conexus")
    expect(deriveMount("/conexus/app/foo/?page=tasks")).toBe("/conexus")
    expect(deriveMount("/conexus/api/foo/all-data")).toBe("/conexus")
    expect(deriveMount("/conexus/assets/chunk.js")).toBe("/conexus")
    expect(deriveMount("/conexus/login")).toBe("/conexus")
  })

  it("returns '' for the root front door (Traefik at host root)", () => {
    expect(deriveMount("/app/foo/")).toBe("")
    expect(deriveMount("/app/")).toBe("")
    expect(deriveMount("/api/foo/all-data")).toBe("")
    expect(deriveMount("/login")).toBe("")
  })

  it("supports an arbitrary proxy-chosen prefix", () => {
    expect(deriveMount("/tools/conexus/app/foo/")).toBe("/tools/conexus")
  })

  it("falls back to /conexus with no window (SSR/prerender)", () => {
    // In vitest's node env there is no window; the no-arg call uses the
    // SSR default so build-time prerender is unaffected.
    expect(deriveMount()).toBe("/conexus")
  })
})
