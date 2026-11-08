import assert from "node:assert/strict";
import test from "node:test";
import { createElement } from "react";
import { AgentMessageCard } from "../src/components/AgentMessageCard.tsx";
import { parseGoal } from "../src/composerCommands.ts";
import { fmtTimeLeft } from "../src/i18n.ts";
import { m } from "../src/paraglide/messages.js";
import { render } from "./render.mjs";

const HOUR = 3_600_000;

test("a goal led by a time is pursued for that long, and any other goal is a reminder", () => {
  assert.deepEqual(parseGoal("7d: halve p50 latency"), { goal: "halve p50 latency", durationMs: 7 * 24 * HOUR });
  assert.deepEqual(parseGoal(" 12H:beat the baseline "), { goal: "beat the baseline", durationMs: 12 * HOUR });
  assert.deepEqual(parseGoal("1.5h : sweep LR"), { goal: "sweep LR", durationMs: 1.5 * HOUR });
  assert.deepEqual(parseGoal("2w: ship it"), { goal: "ship it", durationMs: 14 * 24 * HOUR });
  for (const plain of ["3d reconstruction on ShapeNet", "0d: nothing", "7d:", "improve reasoning: by 5pp"]) {
    assert.deepEqual(parseGoal(plain), { goal: plain.trim(), durationMs: null });
  }
});

test("time left reads as its largest whole unit and never as zero", () => {
  assert.equal(fmtTimeLeft(6.5 * 24 * HOUR), "6d");
  assert.equal(fmtTimeLeft(5 * HOUR + 12 * 60_000), "5h");
  assert.equal(fmtTimeLeft(12 * 60_000 + 59_000), "12m");
  assert.equal(fmtTimeLeft(-1), "1m");
});

test("orx's message keeping a chat at its goal comes from OpenResearch, not another chat", async () => {
  const part = { id: "goal", type: "text", text: "Start working toward the goal.", orx: { kind: "message" } };
  const { container } = await render(createElement(AgentMessageCard, { part }));
  assert.ok(container.textContent.includes(m.agent_message_openresearch()));
  assert.ok(!container.textContent.includes(m.agent_message_other_chat()));
  assert.ok(container.textContent.includes("Start working toward the goal."));
});
