import type { HearthEvent } from "@/lib/hearth-api"

export type TimelineItem =
  | { kind: "user"; id: string; text: string }
  | { kind: "event"; id: string; event: HearthEvent }

export type TranscriptEntry =
  | {
      kind: "message"
      id: string
      role: "user" | "assistant"
      content: string
      streaming: boolean
    }
  | {
      kind: "tool"
      id: string
      name: string
      input: string
      output?: string
      state: "input-streaming" | "input-available" | "output-available" | "output-error"
    }
  | { kind: "reasoning"; id: string; content: string; streaming: boolean }
  | { kind: "permission"; id: string; prompt: unknown }
  | { kind: "error"; id: string; message: string }

const stringField = (event: HearthEvent, key: string) => {
  const value = event[key]
  return typeof value === "string" ? value : ""
}

const rawThought = (event: HearthEvent) => {
  if (event.type !== "RAW" || !event.event || typeof event.event !== "object") {
    return undefined
  }
  const raw = event.event as Record<string, unknown>
  if (raw.sessionUpdate !== "agent_thought_chunk") return undefined
  const content = raw.content
  if (!content || typeof content !== "object") return undefined
  const text = (content as Record<string, unknown>).text
  return typeof text === "string" ? text : undefined
}

export const buildTranscript = (timeline: TimelineItem[]) => {
  const entries: TranscriptEntry[] = []
  const messages = new Map<string, Extract<TranscriptEntry, { kind: "message" }>>()
  const tools = new Map<string, Extract<TranscriptEntry, { kind: "tool" }>>()
  let reasoning: Extract<TranscriptEntry, { kind: "reasoning" }> | undefined
  const finishReasoning = () => {
    if (reasoning) reasoning.streaming = false
    reasoning = undefined
  }

  for (const item of timeline) {
    if (item.kind === "user") {
      const entry: TranscriptEntry = {
        kind: "message",
        id: item.id,
        role: "user",
        content: item.text,
        streaming: false,
      }
      entries.push(entry)
      finishReasoning()
      continue
    }

    const event = item.event
    const messageId = stringField(event, "messageId")
    const toolCallId = stringField(event, "toolCallId")

    switch (event.type) {
      case "TEXT_MESSAGE_START": {
        const entry: Extract<TranscriptEntry, { kind: "message" }> = {
          kind: "message",
          id: messageId || item.id,
          role: stringField(event, "role") === "user" ? "user" : "assistant",
          content: "",
          streaming: true,
        }
        messages.set(entry.id, entry)
        entries.push(entry)
        finishReasoning()
        break
      }
      case "TEXT_MESSAGE_CONTENT": {
        let entry = messages.get(messageId)
        if (!entry) {
          entry = {
            kind: "message",
            id: messageId || item.id,
            role: "assistant",
            content: "",
            streaming: true,
          }
          messages.set(entry.id, entry)
          entries.push(entry)
        }
        entry.content += stringField(event, "delta")
        finishReasoning()
        break
      }
      case "TEXT_MESSAGE_END": {
        const entry = messages.get(messageId)
        if (entry) entry.streaming = false
        break
      }
      case "TOOL_CALL_START": {
        const entry: Extract<TranscriptEntry, { kind: "tool" }> = {
          kind: "tool",
          id: toolCallId || item.id,
          name: stringField(event, "toolCallName") || "tool",
          input: "",
          state: "input-streaming",
        }
        tools.set(entry.id, entry)
        entries.push(entry)
        finishReasoning()
        break
      }
      case "TOOL_CALL_ARGS": {
        const entry = tools.get(toolCallId)
        if (entry) entry.input += stringField(event, "delta")
        break
      }
      case "TOOL_CALL_END": {
        const entry = tools.get(toolCallId)
        if (entry) entry.state = "input-available"
        break
      }
      case "TOOL_CALL_RESULT": {
        let entry = tools.get(toolCallId)
        if (!entry) {
          entry = {
            kind: "tool",
            id: toolCallId || item.id,
            name: "tool",
            input: "",
            state: "output-available",
          }
          tools.set(entry.id, entry)
          entries.push(entry)
        }
        entry.output = stringField(event, "content")
        entry.state = "output-available"
        finishReasoning()
        break
      }
      case "CUSTOM": {
        if (event.name === "hearth.permission_request") {
          entries.push({
            kind: "permission",
            id: item.id,
            prompt: event.value,
          })
        }
        finishReasoning()
        break
      }
      case "RUN_ERROR": {
        entries.push({
          kind: "error",
          id: item.id,
          message: stringField(event, "message") || "The run failed",
        })
        finishReasoning()
        break
      }
      default: {
        const thought = rawThought(event)
        if (thought) {
          if (!reasoning) {
            reasoning = {
              kind: "reasoning",
              id: item.id,
              content: "",
              streaming: true,
            }
            entries.push(reasoning)
          }
          reasoning.content += thought
        } else if (event.type === "RUN_FINISHED" || event.type === "RUN_ERROR") {
          finishReasoning()
        }
      }
    }
  }

  return entries.filter((entry) => entry.kind !== "message" || entry.content.length > 0)
}

export const permissionText = (prompt: unknown) => {
  if (typeof prompt === "string") return prompt
  if (prompt && typeof prompt === "object") {
    const value = prompt as Record<string, unknown>
    for (const key of ["prompt", "message", "description", "title"]) {
      if (typeof value[key] === "string") return value[key]
    }
  }
  return "The agent needs your approval before it can continue."
}
