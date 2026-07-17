import {
  AlertTriangleIcon,
  BotIcon,
  CheckIcon,
  CircleStopIcon,
  Clock3Icon,
  FlameIcon,
  MessageSquarePlusIcon,
  RotateCcwIcon,
  ShieldQuestionIcon,
  XIcon,
} from "lucide-react"

import {
  Conversation,
  ConversationContent,
  ConversationEmptyState,
  ConversationScrollButton,
} from "@/components/ai-elements/conversation"
import { Message, MessageContent, MessageResponse } from "@/components/ai-elements/message"
import {
  PromptInput,
  PromptInputBody,
  PromptInputFooter,
  PromptInputSubmit,
  PromptInputTextarea,
  PromptInputTools,
} from "@/components/ai-elements/prompt-input"
import {
  Reasoning,
  ReasoningContent,
  ReasoningTrigger,
} from "@/components/ai-elements/reasoning"
import { Tool, ToolContent, ToolHeader, ToolInput, ToolOutput } from "@/components/ai-elements/tool"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Separator } from "@/components/ui/separator"
import { cn } from "@/lib/utils"
import type { AgentInfo, TaskState, TaskSummary } from "@/lib/hearth-api"
import { permissionText, type TranscriptEntry } from "@/lib/transcript"

interface AgentWorkspaceProps {
  agent?: AgentInfo
  task?: TaskSummary
  transcript: TranscriptEntry[]
  busy: boolean
  composerKey: string
  streamError?: string
  onSubmit: (text: string) => Promise<void>
  onStop: () => void
  onCancel: () => void
  onNewTask: () => void
  onQuickResponse: (text: string) => void
  onRetryReplay: () => void
}

const stateLabel: Record<TaskState, string> = {
  queued: "Queued",
  running: "Running",
  awaiting_input: "Needs input",
  completed: "Completed",
  failed: "Failed",
  canceled: "Canceled",
}

const stateClass: Record<TaskState, string> = {
  queued: "border-sky-500/20 bg-sky-500/10 text-sky-300",
  running: "border-amber-400/20 bg-amber-400/10 text-amber-300",
  awaiting_input: "border-violet-400/20 bg-violet-400/10 text-violet-300",
  completed: "border-emerald-400/20 bg-emerald-400/10 text-emerald-300",
  failed: "border-red-400/20 bg-red-400/10 text-red-300",
  canceled: "border-neutral-700 bg-neutral-800 text-neutral-400",
}

const parseToolInput = (value: string) => {
  if (!value) return {}
  try {
    return JSON.parse(value) as unknown
  } catch {
    return value
  }
}

function Entry({ entry }: { entry: TranscriptEntry }) {
  if (entry.kind === "message") {
    return (
      <Message className="max-w-[88%]" from={entry.role}>
        <MessageContent
          className={cn(
            entry.role === "user" &&
              "border border-neutral-700 bg-neutral-800 text-neutral-100 shadow-sm",
          )}
        >
          <MessageResponse isAnimating={entry.streaming}>{entry.content}</MessageResponse>
        </MessageContent>
      </Message>
    )
  }

  if (entry.kind === "tool") {
    return (
      <Tool className="border-neutral-800 bg-neutral-950/60" defaultOpen={false}>
        <ToolHeader
          state={entry.state}
          title={entry.name}
          toolName={entry.name}
          type="dynamic-tool"
        />
        <ToolContent className="border-t border-neutral-800">
          <ToolInput input={parseToolInput(entry.input)} />
          <ToolOutput errorText={undefined} output={entry.output} />
        </ToolContent>
      </Tool>
    )
  }

  if (entry.kind === "reasoning") {
    return (
      <Reasoning className="rounded-lg border border-neutral-800 bg-neutral-950/40 p-3" isStreaming={entry.streaming}>
        <ReasoningTrigger />
        <ReasoningContent>{entry.content}</ReasoningContent>
      </Reasoning>
    )
  }

  if (entry.kind === "permission") {
    return (
      <div className="rounded-xl border border-violet-400/20 bg-violet-400/5 p-4">
        <div className="flex items-start gap-3">
          <div className="mt-0.5 rounded-lg bg-violet-400/10 p-2 text-violet-300">
            <ShieldQuestionIcon className="size-4" />
          </div>
          <div className="min-w-0 space-y-1">
            <p className="text-sm font-medium text-violet-200">Approval requested</p>
            <p className="text-sm leading-6 text-neutral-400">{permissionText(entry.prompt)}</p>
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="flex items-start gap-2 rounded-xl border border-red-400/20 bg-red-400/5 p-4 text-sm text-red-300">
      <AlertTriangleIcon className="mt-0.5 size-4 shrink-0" />
      <span>{entry.message}</span>
    </div>
  )
}

export function AgentWorkspace({
  agent,
  task,
  transcript,
  busy,
  composerKey,
  streamError,
  onSubmit,
  onStop,
  onCancel,
  onNewTask,
  onQuickResponse,
  onRetryReplay,
}: AgentWorkspaceProps) {
  const canRespond = !task || task.state === "awaiting_input"
  const canCancel = task && ["queued", "running", "awaiting_input"].includes(task.state)
  const status = busy ? "streaming" : streamError ? "error" : "ready"

  return (
    <main className="flex min-h-0 min-w-0 flex-col bg-neutral-900 text-neutral-100">
      <header className="flex h-16 shrink-0 items-center justify-between gap-4 border-b border-neutral-800 px-4 sm:px-6">
        <div className="flex min-w-0 items-center gap-3">
          <div className="flex size-8 shrink-0 items-center justify-center rounded-xl border border-neutral-700 bg-neutral-800 text-neutral-300">
            <BotIcon className="size-4" />
          </div>
          <div className="min-w-0">
            <div className="flex items-center gap-2">
              <h1 className="truncate text-sm font-semibold">{agent?.name ?? task?.agent_vm ?? "Choose an agent"}</h1>
              {task ? (
                <Badge className={cn("h-5 border text-[10px]", stateClass[task.state])} variant="outline">
                  {task.state === "running" ? <span className="size-1.5 animate-pulse rounded-full bg-current" /> : null}
                  {stateLabel[task.state]}
                </Badge>
              ) : null}
            </div>
            <p className="truncate text-xs text-neutral-500">
              {task
                ? `${task.agent} · ${task.runs.length} ${task.runs.length === 1 ? "run" : "runs"} · ${task.task_id.slice(0, 12)}`
                : agent
                  ? `${agent.adapters.join(", ")} · new task`
                  : "Select a ready VM from the fleet"}
            </p>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-1">
          {task && agent ? (
            <Button
              className="text-neutral-400 hover:bg-neutral-800 hover:text-neutral-100"
              onClick={onNewTask}
              size="sm"
              variant="ghost"
            >
              <MessageSquarePlusIcon data-icon="inline-start" /> New task
            </Button>
          ) : null}
          {canCancel ? (
            <Button
              className="text-neutral-400 hover:bg-red-950/40 hover:text-red-300"
              onClick={onCancel}
              size="sm"
              variant="ghost"
            >
              <CircleStopIcon data-icon="inline-start" /> Cancel task
            </Button>
          ) : null}
        </div>
      </header>

      <Conversation className="min-h-0">
        <ConversationContent className="mx-auto w-full max-w-3xl gap-6 px-4 py-8 sm:px-6">
          {transcript.length === 0 ? (
            <ConversationEmptyState
              description={
                agent
                  ? `Send a task to ${agent.name}. The run will stream here and remain replayable.`
                  : "Pick a ready agent to start a durable task, or open one from history."
              }
              icon={
                <div className="flex size-12 items-center justify-center rounded-2xl border border-amber-300/15 bg-amber-300/5 text-amber-300">
                  <FlameIcon className="size-5" />
                </div>
              }
              title={agent ? "What should the agent do?" : "The fleet is waiting"}
            />
          ) : (
            transcript.map((entry) => <Entry entry={entry} key={`${entry.kind}-${entry.id}`} />)
          )}

          {streamError ? (
            <div className="flex items-center justify-between gap-4 rounded-xl border border-red-400/20 bg-red-400/5 px-4 py-3">
              <div className="flex min-w-0 items-center gap-2 text-sm text-red-300">
                <AlertTriangleIcon className="size-4 shrink-0" />
                <span className="truncate">{streamError}</span>
              </div>
              {task ? (
                <Button className="shrink-0" onClick={onRetryReplay} size="sm" variant="outline">
                  <RotateCcwIcon data-icon="inline-start" /> Retry
                </Button>
              ) : null}
            </div>
          ) : null}
        </ConversationContent>
        <ConversationScrollButton className="border-neutral-700 bg-neutral-900" />
      </Conversation>

      <div className="shrink-0 border-t border-neutral-800 bg-neutral-900/95 px-4 py-4 sm:px-6">
        <div className="mx-auto max-w-3xl space-y-3">
          {task?.state === "awaiting_input" ? (
            <div className="flex flex-wrap items-center gap-2 rounded-xl border border-violet-400/20 bg-violet-400/5 px-3 py-2.5">
              <div className="mr-auto flex min-w-0 items-center gap-2 text-xs text-violet-200">
                <Clock3Icon className="size-3.5 shrink-0" />
                <span className="truncate">The agent is waiting for your decision.</span>
              </div>
              <Button onClick={() => onQuickResponse("deny")} size="sm" variant="ghost">
                <XIcon data-icon="inline-start" /> Deny
              </Button>
              <Button
                className="bg-violet-300 text-neutral-950 hover:bg-violet-200"
                onClick={() => onQuickResponse("allow")}
                size="sm"
              >
                <CheckIcon data-icon="inline-start" /> Allow
              </Button>
            </div>
          ) : null}
          <PromptInput
            className="rounded-2xl border-neutral-700 bg-neutral-950/70 shadow-xl shadow-black/10"
            key={composerKey}
            onSubmit={async ({ text }) => {
              if (text.trim()) await onSubmit(text.trim())
            }}
          >
            <PromptInputBody>
              <PromptInputTextarea
                disabled={!agent || (!canRespond && !busy)}
                placeholder={
                  task?.state === "awaiting_input"
                    ? "Answer the agent…"
                    : task
                      ? "This task is not waiting for another turn"
                      : agent
                        ? `Give ${agent.name} a task…`
                        : "Choose an agent to begin"
                }
              />
            </PromptInputBody>
            <PromptInputFooter>
              <PromptInputTools>
                <span className="px-1 text-[11px] text-neutral-600">
                  {task?.state === "awaiting_input" ? "New run on the same thread" : "Enter to send · Shift+Enter for newline"}
                </span>
              </PromptInputTools>
              <PromptInputSubmit
                disabled={!agent || (!canRespond && !busy)}
                onStop={onStop}
                status={status}
              />
            </PromptInputFooter>
          </PromptInput>
          <div className="flex items-center justify-center gap-2 text-[10px] text-neutral-600">
            <span>Streams over AG-UI</span>
            <Separator className="h-3 bg-neutral-800" orientation="vertical" />
            <span>Task history is durable in the guest</span>
          </div>
        </div>
      </div>
    </main>
  )
}
