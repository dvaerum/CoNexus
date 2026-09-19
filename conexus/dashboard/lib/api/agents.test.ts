import { describe, it, expect } from "vitest"
import { agentPresence, type Agent, type AgentPresence } from "@/lib/api"

// Regression lock for docs/learnings/dashboard-known-gaps.md's
// "Agents tab counters don't match the row statuses" entry (verified
// RESOLVED while fixing the router's project-list agent-count
// mismatch -- see rust/conexus-router/src/project_reads.rs).
//
// The bug: the Agents tab's Total/Online/Pending/Offline summary
// could disagree with its own row count because some `Agent.status`
// values (e.g. `system`) fell outside every bucket `agentPresence()`
// covered. `agentPresence()` must be a TOTAL function -- every agent,
// whatever its backend `status` string, resolves to exactly one of the
// four presence buckets the Agents tab sums back into `Total`.

const baseAgent: Agent = {
  agent_id: "a1",
  status: "pending",
  created_at: "2026-01-01T00:00:00Z",
  updated_at: "2026-01-01T00:00:00Z",
}

function agentWith(overrides: Partial<Agent>): Agent {
  return { ...baseAgent, ...overrides }
}

describe("agentPresence (total function over Agent.status)", () => {
  it("maps terminated status to the terminated bucket regardless of online/last_mcp_connection", () => {
    expect(
      agentPresence(
        agentWith({ status: "terminated", online: true } as Agent),
      ),
    ).toBe("terminated")
  })

  it("maps a live MCP stream to online", () => {
    expect(agentPresence(agentWith({ online: true } as Agent))).toBe("online")
  })

  it("maps previously-connected, not-live to offline", () => {
    expect(
      agentPresence(
        agentWith({
          online: false,
          last_mcp_connection: "2026-01-01T00:00:00Z",
        } as Agent),
      ),
    ).toBe("offline")
  })

  it("maps never-connected to pending, including backend status values the frontend union doesn't name (e.g. 'system')", () => {
    // Real DB status values include 'active'/'created'/'system' (see
    // rust/conexus-db's agent schema) -- none of these are `terminated`,
    // so they must all fall through to a live-presence bucket rather
    // than being silently dropped.
    for (const status of ["active", "created", "system", "unknown-future-status"]) {
      expect(agentPresence(agentWith({ status } as Agent["status"] & string))).toBe(
        "pending",
      )
    }
  })

  it("buckets every agent into exactly one of the four presence states -- Total always equals the bucket sum", () => {
    const agents: Agent[] = [
      agentWith({ agent_id: "online1", online: true } as Agent),
      agentWith({ agent_id: "pending1" }),
      agentWith({
        agent_id: "offline1",
        last_mcp_connection: "2026-01-01T00:00:00Z",
      } as Agent),
      agentWith({ agent_id: "terminated1", status: "terminated" }),
      agentWith({ agent_id: "terminated2", status: "terminated" }),
      // A 'system' agent -- the exact repro shape from the known-gaps
      // entry (1 system agent + 2 terminated, Total should still be 3
      // once combined with the terminated rows above).
      agentWith({ agent_id: "system1", status: "system" } as Agent),
    ]

    const buckets: Record<AgentPresence, number> = {
      online: 0,
      pending: 0,
      offline: 0,
      terminated: 0,
    }
    for (const agent of agents) {
      buckets[agentPresence(agent)] += 1
    }

    const total = Object.values(buckets).reduce((a, b) => a + b, 0)
    expect(total).toBe(agents.length)
    expect(buckets).toEqual({ online: 1, pending: 2, offline: 1, terminated: 2 })
  })
})
