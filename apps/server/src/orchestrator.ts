/**
 * TypeScript orchestration layer.
 *
 * The Rust runtime executes ONE agent loop per session. Orchestration - splitting a goal into
 * independent sub-goals, running them in parallel sessions and aggregating the answers - is an
 * application concern and lives here, where it can change without touching the runtime.
 *
 * Everything below talks to the runtime through the same public HTTP API a browser would use.
 */

import type { EventRecord, RunResult, RuntimeClient, SessionSummary } from "./client.ts";

export interface SubGoal {
  id: string;
  text: string;
}

export interface SubGoalResult {
  id: string;
  sessionId: string;
  state: string;
  answer: string | null;
  error: string | null;
  durationMs: number;
}

export interface OrchestrationReport {
  goal: string;
  strategy: "single" | "fan-out";
  subGoals: SubGoal[];
  results: SubGoalResult[];
  summary: string;
  events: number;
  durationMs: number;
}

/**
 * Split a goal into sub-goals. Deterministic on purpose: the model-driven variant is
 * planWithRuntime below, and this one is the fallback that never needs the network.
 */
export function splitGoal(goal: string, maxParts = 3): SubGoal[] {
  const explicit = goal
    .split(/\n+|;\s*|(?:\d+[).]\s*)/)
    .map((part) => part.trim())
    .filter((part) => part.length > 0);
  const parts = explicit.length > 1 ? explicit : [goal.trim()];
  return parts.slice(0, maxParts).map((text, index) => ({ id: "sub-" + (index + 1), text }));
}

/** Ask the runtime's own planner to decompose a goal, then run the parts. */
export async function planWithRuntime(client: RuntimeClient, goal: string): Promise<SubGoal[]> {
  try {
    const sessions = await client.listSessions();
    if (sessions.length === 0) return splitGoal(goal);
    const planner = sessions[0];
    const result = await client.postGoal(
      planner.id,
      "Split this task into at most 3 independent sub-goals and list them as plain lines: " + goal,
      true,
    );
    if (!result.answer) return splitGoal(goal);
    const lines = result.answer
      .split("\n")
      .map((line) => line.replace(/^[-*\d.\s)]+/, "").trim())
      .filter((line) => line.length > 3);
    if (lines.length === 0) return splitGoal(goal);
    return lines.slice(0, 3).map((text, index) => ({ id: "sub-" + (index + 1), text }));
  } catch {
    return splitGoal(goal);
  }
}

export async function orchestrate(
  client: RuntimeClient,
  goal: string,
  options: { strategy?: "single" | "fan-out"; userId?: string } = {},
): Promise<OrchestrationReport> {
  const started = Date.now();
  const strategy = options.strategy ?? "fan-out";
  const userId = options.userId ?? "ts-orchestrator";
  const subGoals = strategy === "single" ? [{ id: "single", text: goal }] : splitGoal(goal);
  const eventsBefore = await countEvents(client);

  // One session per sub-goal: sessions are independent, so this is genuinely parallel work.
  const sessions: SessionSummary[] = [];
  for (const sub of subGoals) {
    sessions.push(await client.createSession(userId, "orchestrated: " + sub.text.slice(0, 48)));
  }

  const results = await Promise.all(
    subGoals.map(async (sub, index) => {
      const session = sessions[index];
      const subStarted = Date.now();
      try {
        const run: RunResult = await client.postGoal(session.id, sub.text, true);
        const ok: SubGoalResult = {
          id: sub.id,
          sessionId: session.id,
          state: run.state,
          answer: run.answer,
          error: run.error,
          durationMs: Date.now() - subStarted,
        };
        return ok;
      } catch (error) {
        const failed: SubGoalResult = {
          id: sub.id,
          sessionId: session.id,
          state: "failed",
          answer: null,
          error: error instanceof Error ? error.message : String(error),
          durationMs: Date.now() - subStarted,
        };
        return failed;
      }
    }),
  );

  const eventsAfter = await countEvents(client);
  const succeeded = results.filter((r) => r.error === null).length;
  return {
    goal,
    strategy,
    subGoals,
    results,
    summary:
      succeeded + "/" + results.length + " sub-goals completed; answers: " +
      results.map((r) => r.answer ?? ("[" + (r.error ?? "no answer") + "]")).join(" | "),
    events: Math.max(0, eventsAfter - eventsBefore),
    durationMs: Date.now() - started,
  };
}

async function countEvents(client: RuntimeClient): Promise<number> {
  try {
    const events: EventRecord[] = await client.listEvents(1000);
    return events.length;
  } catch {
    return 0;
  }
}
