import type { HearthEvent } from "@/lib/hearth-api"

export type TimelineItem = { kind: "event"; id: string; event: HearthEvent }

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

export const buildTranscript = (timeline: TimelineItem[]) => {
  const entries: TranscriptEntry[] = []
  const messages = new Map<string, Extract<TranscriptEntry, { kind: "message" }>>()
  const tools = new Map<string, Extract<TranscriptEntry, { kind: "tool" }>>()
  let reasoning: Extract<TranscriptEntry, { kind: "reasoning" }> | undefined
  const finishReasoning = () => {
    if (reasoning) reasoning.streaming = false
    reasoning = undefined
  }
  const finishMessages = () => {
    for (const message of messages.values()) message.streaming = false
  }

  for (const item of timeline) {
    const event = item.event
    const messageId = stringField(event, "messageId")
    const toolCallId = stringField(event, "toolCallId")

    switch (event.type) {
      case "TEXT_MESSAGE_START": {
        if (!messageId) break
        const entry: Extract<TranscriptEntry, { kind: "message" }> = {
          kind: "message",
          id: messageId,
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
        const entry = messages.get(messageId)
        if (entry) entry.content += stringField(event, "delta")
        finishReasoning()
        break
      }
      case "TEXT_MESSAGE_END": {
        const entry = messages.get(messageId)
        if (entry) entry.streaming = false
        break
      }
      case "REASONING_START": {
        break
      }
      case "REASONING_MESSAGE_START": {
        if (!messageId) break
        finishReasoning()
        reasoning = {
          kind: "reasoning",
          id: messageId,
          content: "",
          streaming: true,
        }
        entries.push(reasoning)
        break
      }
      case "REASONING_MESSAGE_CONTENT": {
        if (reasoning?.id === messageId) reasoning.content += stringField(event, "delta")
        break
      }
      case "REASONING_MESSAGE_END": {
        if (reasoning?.id === messageId) finishReasoning()
        break
      }
      case "REASONING_END": {
        finishReasoning()
        break
      }
      case "TOOL_CALL_START": {
        const toolCallName = stringField(event, "toolCallName")
        if (!toolCallId || !toolCallName) break
        const entry: Extract<TranscriptEntry, { kind: "tool" }> = {
          kind: "tool",
          id: toolCallId,
          name: toolCallName,
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
        const entry = tools.get(toolCallId)
        if (entry) {
          entry.output = stringField(event, "content")
          entry.state = "output-available"
        }
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
        finishMessages()
        finishReasoning()
        break
      }
      case "RUN_FINISHED": {
        finishMessages()
        finishReasoning()
        break
      }
      default: {
        break
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
