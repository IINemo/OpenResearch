import { SPEEDUP } from '../src/orx.ts'
import { expect, send, test } from '../src/test.ts'

/** How long a chat stays quiet before OpenResearch starts a turn toward its goal, sped up as on every e2e server. */
const QUIET_MS = 120_000 / SPEEDUP

const goalMet = `printf '%s\\n' '{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"goal","arguments":{"action":"met","evidence":"p50 latency fell 52% in the last round"}}}' | orx mcp`

test('a goal with a deadline keeps the agent working on its own until it reports the goal met', async ({ page, project, agent }) => {
  agent.script('You ended your last turn with nothing in flight', [{ run: goalMet }, { say: 'The goal is met: p50 latency fell 52%.' }])
  agent.script('Halve p50 latency.', [{ say: 'Measured the baseline latency.' }])
  await project.open(page)
  await send(page, '/goal 7d: Halve p50 latency.')
  const chip = page.getByRole('button', { name: /Clear the goal/ })

  await test.step('the goal starts the first turn and counts down to its deadline', async () => {
    await expect(page.getByText('Measured the baseline latency.')).toBeVisible()
    await expect(chip).toContainText('Goal: 6d left')
  })

  await test.step('once the chat is quiet, OpenResearch starts the next turn and the agent reports the goal met', async () => {
    // Waits out the goal keeper's quiet period.
    await expect(page.getByText('The goal is met: p50 latency fell 52%.')).toBeVisible({ timeout: QUIET_MS + 15_000 })
    await expect(chip).toContainText('Goal met')
    await expect(page.getByRole('feed', { name: 'Messages' }).getByText('OpenResearch', { exact: true })).toBeVisible()
  })
})
