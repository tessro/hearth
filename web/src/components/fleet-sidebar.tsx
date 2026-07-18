import {
  BotIcon,
  CheckCircle2Icon,
  CircleOffIcon,
  FlameIcon,
  HistoryIcon,
  LogOutIcon,
  MessageSquarePlusIcon,
  RefreshCwIcon,
} from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Separator } from "@/components/ui/separator"
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip"
import { TypingSessionName } from "@/components/typing-session-name"
import { cn } from "@/lib/utils"
import type { AgentInfo, TaskState, TaskSummary } from "@/lib/hearth-api"

interface FleetSidebarProps {
  agents: AgentInfo[]
  tasks: TaskSummary[]
  selectedAgent?: string
  selectedTaskId?: string
  refreshing: boolean
  onNewTask: (agent: AgentInfo) => void
  onOpenTask: (task: TaskSummary) => void
  onRefresh: () => void
  onDisconnect: () => void
}

const stateDot: Record<TaskState, string> = {
  queued: "bg-sky-400",
  running: "bg-amber-300 animate-pulse",
  awaiting_input: "bg-violet-400",
  completed: "bg-emerald-400",
  failed: "bg-red-400",
  canceled: "bg-neutral-500",
}

const relativeTime = (value: string) => {
  const timestamp = Date.parse(value)
  if (Number.isNaN(timestamp)) return value
  const seconds = Math.round((timestamp - Date.now()) / 1000)
  const formatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" })
  if (Math.abs(seconds) < 60) return formatter.format(seconds, "second")
  const minutes = Math.round(seconds / 60)
  if (Math.abs(minutes) < 60) return formatter.format(minutes, "minute")
  const hours = Math.round(minutes / 60)
  if (Math.abs(hours) < 24) return formatter.format(hours, "hour")
  return formatter.format(Math.round(hours / 24), "day")
}

export function FleetSidebar({
  agents,
  tasks,
  selectedAgent,
  selectedTaskId,
  refreshing,
  onNewTask,
  onOpenTask,
  onRefresh,
  onDisconnect,
}: FleetSidebarProps) {
  return (
    <aside className="flex min-h-0 flex-col border-b border-neutral-800 bg-neutral-950 text-neutral-200 md:border-r md:border-b-0">
      <div className="flex h-16 shrink-0 items-center justify-between px-4">
        <div className="flex items-center gap-2.5">
          <div className="flex size-8 items-center justify-center rounded-xl bg-amber-300/10 text-amber-300">
            <FlameIcon className="size-4" />
          </div>
          <div>
            <p className="text-sm font-semibold">Hearth</p>
            <p className="text-[11px] text-neutral-500">Agent console</p>
          </div>
        </div>
        <div className="flex items-center">
          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  aria-label="Refresh fleet"
                  className="text-neutral-500 hover:bg-neutral-900 hover:text-neutral-200"
                  onClick={onRefresh}
                  size="icon-sm"
                  variant="ghost"
                />
              }
            >
              <RefreshCwIcon className={cn("size-3.5", refreshing && "animate-spin")} />
            </TooltipTrigger>
            <TooltipContent>Refresh fleet</TooltipContent>
          </Tooltip>
          <Tooltip>
            <TooltipTrigger
              render={
                <Button
                  aria-label="Disconnect"
                  className="text-neutral-500 hover:bg-neutral-900 hover:text-neutral-200"
                  onClick={onDisconnect}
                  size="icon-sm"
                  variant="ghost"
                />
              }
            >
              <LogOutIcon className="size-3.5" />
            </TooltipTrigger>
            <TooltipContent>Disconnect</TooltipContent>
          </Tooltip>
        </div>
      </div>
      <Separator className="bg-neutral-800" />

      <div className="grid min-h-0 flex-1 grid-cols-2 overflow-hidden md:flex md:flex-col">
        <section className="min-h-0 overflow-y-auto border-r border-neutral-800 p-3 md:max-h-[40%] md:border-r-0 md:border-b">
          <div className="mb-2 flex items-center justify-between px-1">
            <p className="flex items-center gap-1.5 text-[11px] font-medium tracking-wider text-neutral-500 uppercase">
              <BotIcon className="size-3" /> Agents
            </p>
            <span className="text-[11px] text-neutral-600">{agents.length}</span>
          </div>
          <div className="space-y-1.5">
            {agents.map((agent) => {
              const available = agent.running && agent.ready && agent.adapters.length > 0
              return (
                <button
                  className={cn(
                    "group w-full rounded-xl border px-3 py-2.5 text-left transition-colors",
                    selectedAgent === agent.name
                      ? "border-amber-300/20 bg-amber-300/10"
                      : "border-transparent hover:border-neutral-800 hover:bg-neutral-900/80",
                    !available && "opacity-55",
                  )}
                  disabled={!available}
                  key={agent.name}
                  onClick={() => onNewTask(agent)}
                  type="button"
                >
                  <div className="flex items-center justify-between gap-2">
                    <span className="truncate text-sm font-medium">{agent.name}</span>
                    {available ? (
                      <CheckCircle2Icon className="size-3.5 text-emerald-400" />
                    ) : (
                      <CircleOffIcon className="size-3.5 text-neutral-600" />
                    )}
                  </div>
                  <div className="mt-1.5 flex items-center gap-1.5">
                    <span className="truncate text-xs text-neutral-500">
                      {agent.adapters.join(", ") || "unavailable"}
                    </span>
                    {available ? (
                      <MessageSquarePlusIcon className="ml-auto size-3 text-neutral-600 group-hover:text-amber-300" />
                    ) : null}
                  </div>
                </button>
              )
            })}
            {agents.length === 0 ? (
              <p className="px-3 py-6 text-center text-xs text-neutral-600">No agents discovered</p>
            ) : null}
          </div>
        </section>

        <section className="min-h-0 overflow-y-auto p-3">
          <div className="mb-2 flex items-center justify-between px-1">
            <p className="flex items-center gap-1.5 text-[11px] font-medium tracking-wider text-neutral-500 uppercase">
              <HistoryIcon className="size-3" /> Recent tasks
            </p>
            <span className="text-[11px] text-neutral-600">{tasks.length}</span>
          </div>
          <div className="space-y-1">
            {tasks.map((task) => (
              <button
                className={cn(
                  "w-full rounded-xl px-3 py-2.5 text-left transition-colors",
                  selectedTaskId === task.task_id
                    ? "bg-neutral-800 text-neutral-100"
                    : "hover:bg-neutral-900/80",
                )}
                key={task.task_id}
                onClick={() => onOpenTask(task)}
                type="button"
              >
                <div className="flex items-center gap-2">
                  <span className={cn("size-1.5 shrink-0 rounded-full", stateDot[task.state])} />
                  <TypingSessionName
                    className="min-w-0 flex-1 text-xs font-medium"
                    name={(task.session_name ?? task.text) || `Task ${task.task_id.slice(0, 8)}`}
                  />
                </div>
                <div className="mt-1.5 flex items-center gap-2 pl-3.5 text-[11px] text-neutral-600">
                  <span className="truncate">{task.agent_vm}</span>
                  <span>·</span>
                  <span className="shrink-0">{relativeTime(task.updated_at)}</span>
                </div>
              </button>
            ))}
            {tasks.length === 0 ? (
              <div className="flex flex-col items-center gap-2 px-3 py-8 text-center">
                <HistoryIcon className="size-5 text-neutral-700" />
                <p className="text-xs text-neutral-600">Tasks will appear here</p>
              </div>
            ) : null}
          </div>
        </section>
      </div>
      <div className="hidden border-t border-neutral-800 p-3 md:block">
        <Badge className="w-full justify-center border-neutral-800 bg-neutral-900 text-[10px] text-neutral-500" variant="outline">
          Authenticated · AG-UI
        </Badge>
      </div>
    </aside>
  )
}
