import assert from "node:assert/strict"
import { randomUUID } from "node:crypto"

import { HttpAgent } from "@ag-ui/client"

const [url, token] = process.argv.slice(2)
assert.ok(url, "usage: verify-http-agent.mjs <url> <token>")
assert.ok(token, "missing bearer token")

const threadId = randomUUID()

async function run(text, taskRef) {
  const events = []
  const agent = new HttpAgent({
    agentId: "worker",
    headers: { Authorization: `Bearer ${token}` },
    initialMessages: [
      { id: randomUUID(), role: "user", content: text },
    ],
    threadId,
    url,
  })
  await agent.runAgent(
    taskRef ? { forwardedProps: { task_ref: taskRef } } : undefined,
    { onEvent: ({ event }) => events.push(event) },
  )
  return events
}

function eventTypes(events) {
  return events.map((event) => event.type)
}

function taskRef(events) {
  return events.find(
    (event) => event.type === "CUSTOM" && event.name === "hearth.task_ref",
  )?.value?.task_ref
}

const interrupted = await run("NEEDS_APPROVAL sdk conformance", undefined)
assert.ok(
  interrupted.some(
    (event) =>
      event.type === "CUSTOM" && event.name === "hearth.permission_request",
  ),
  `permission interrupt missing: ${eventTypes(interrupted)}`,
)
const firstRef = taskRef(interrupted)
assert.ok(firstRef, "interrupted run did not return a Hearth task ref")

const resumed = await run("allow", firstRef)
assert.ok(
  eventTypes(resumed).includes("RUN_STARTED") &&
    eventTypes(resumed).includes("RUN_FINISHED"),
  `resume did not finish: ${eventTypes(resumed)}`,
)
const resumedRef = taskRef(resumed)
assert.ok(resumedRef, "resumed run did not refresh the Hearth task ref")

const followedUp = await run("sdk follow-up", resumedRef)
assert.ok(
  eventTypes(followedUp).includes("TEXT_MESSAGE_CONTENT") &&
    eventTypes(followedUp).includes("RUN_FINISHED"),
  `follow-up did not stream and finish: ${eventTypes(followedUp)}`,
)

process.stdout.write("unmodified @ag-ui/client HttpAgent conformance passed\n")
