import { useCallback, useEffect, useMemo, useRef, useState } from "react"
import { HttpAgent, randomUUID, type BaseEvent } from "@ag-ui/client"

import { AgentWorkspace } from "@/components/agent-workspace"
import { ConnectionPanel } from "@/components/connection-panel"
import { FleetSidebar } from "@/components/fleet-sidebar"
import {
  cancelTask,
  endpoint,
  getTask,
  isAbortError,
  listAgents,
  listTasks,
  streamTaskEvents,
  type AgentInfo,
  type ConnectionSettings,
  type HearthEvent,
  type TaskState,
  type TaskSummary,
} from "@/lib/hearth-api"
import { buildTranscript, type TimelineItem } from "@/lib/transcript"

const storedSettings = (): ConnectionSettings => ({
  baseUrl:
    sessionStorage.getItem("hearth.baseUrl") ??
    localStorage.getItem("hearth.baseUrl") ??
    import.meta.env.VITE_HEARTH_API_URL ??
    "",
  token: sessionStorage.getItem("hearth.token") ?? "",
})

const eventState = (event: HearthEvent): TaskState | undefined => {
  if (event.type !== "CUSTOM" || event.name !== "hearth.state") return undefined
  if (!event.value || typeof event.value !== "object") return undefined
  const state = (event.value as Record<string, unknown>).state
  if (
    state === "queued" ||
    state === "running" ||
    state === "awaiting_input" ||
    state === "completed" ||
    state === "failed" ||
    state === "canceled"
  ) {
    return state
  }
  return undefined
}

const eventTaskRef = (event: HearthEvent) => {
  if (event.type !== "CUSTOM" || event.name !== "hearth.task_ref") return undefined
  if (!event.value || typeof event.value !== "object") return undefined
  const taskRef = (event.value as Record<string, unknown>).task_ref
  return typeof taskRef === "string" ? taskRef : undefined
}

const eventSessionName = (event: HearthEvent) => {
  if (event.type !== "CUSTOM" || event.name !== "hearth.session_name") return undefined
  if (!event.value || typeof event.value !== "object") return undefined
  const name = (event.value as Record<string, unknown>).name
  return typeof name === "string" ? name : undefined
}

function App() {
  const [settings, setSettings] = useState<ConnectionSettings>(storedSettings)
  const [connected, setConnected] = useState(false)
  const [connecting, setConnecting] = useState(false)
  const [connectionError, setConnectionError] = useState<string>()
  const [agents, setAgents] = useState<AgentInfo[]>([])
  const [tasks, setTasks] = useState<TaskSummary[]>([])
  const [selectedAgentId, setSelectedAgentId] = useState<string>()
  const [selectedTask, setSelectedTask] = useState<TaskSummary>()
  const [sessionName, setSessionName] = useState<string>()
  const [timeline, setTimeline] = useState<TimelineItem[]>([])
  const [busy, setBusy] = useState(false)
  const [refreshing, setRefreshing] = useState(false)
  const [streamError, setStreamError] = useState<string>()
  const [draftId, setDraftId] = useState(0)
  const replayAbort = useRef<AbortController | undefined>(undefined)
  const activeAgent = useRef<HttpAgent | undefined>(undefined)
  const detachingRun = useRef(false)
  const liveEventId = useRef(0)

  const selectedAgent = useMemo(
    () => agents.find((agent) => agent.id === selectedAgentId),
    [agents, selectedAgentId],
  )
  const transcript = useMemo(() => buildTranscript(timeline), [timeline])
  const displayedSessionName = selectedTask
    ? selectedTask.session_name ?? selectedTask.text
    : sessionName

  const refresh = useCallback(
    async (nextSettings = settings, quiet = false) => {
      if (!quiet) setRefreshing(true)
      try {
        const [nextAgents, nextTasks] = await Promise.all([
          listAgents(nextSettings),
          listTasks(nextSettings),
        ])
        setAgents(nextAgents)
        setTasks(nextTasks)
        setSelectedTask((current) =>
          current ? nextTasks.find((task) => task.task_id === current.task_id) ?? current : current,
        )
        return { agents: nextAgents, tasks: nextTasks }
      } finally {
        if (!quiet) setRefreshing(false)
      }
    },
    [settings],
  )

  const connect = async (nextSettings: ConnectionSettings) => {
    setConnecting(true)
    setConnectionError(undefined)
    try {
      const fleet = await refresh(nextSettings)
      setSettings(nextSettings)
      sessionStorage.setItem("hearth.baseUrl", nextSettings.baseUrl)
      sessionStorage.setItem("hearth.token", nextSettings.token)
      localStorage.setItem("hearth.baseUrl", nextSettings.baseUrl)
      setConnected(true)
      const firstReady = fleet.agents.find(
        (agent) => agent.running && agent.ready && agent.adapters.length > 0,
      )
      setSelectedAgentId(firstReady?.id)
    } catch (error) {
      setConnectionError(error instanceof Error ? error.message : "Could not connect to agentd")
    } finally {
      setConnecting(false)
    }
  }

  const stopStreams = useCallback(() => {
    replayAbort.current?.abort()
    replayAbort.current = undefined
    activeAgent.current?.abortRun()
    activeAgent.current = undefined
  }, [])

  const applyTaskState = useCallback((state: TaskState) => {
    setSelectedTask((current) => (current ? { ...current, state } : current))
    setTasks((current) =>
      current.map((task) => (task.task_id === selectedTask?.task_id ? { ...task, state } : task)),
    )
  }, [selectedTask?.task_id])

  const applySessionName = useCallback((name: string) => {
    setSessionName(name)
    setSelectedTask((current) => (current ? { ...current, session_name: name } : current))
    setTasks((current) =>
      current.map((task) =>
        task.task_id === selectedTask?.task_id ? { ...task, session_name: name } : task,
      ),
    )
  }, [selectedTask?.task_id])

  const appendEvent = useCallback(
    (id: string, event: HearthEvent) => {
      setTimeline((current) => [...current, { kind: "event", id, event }])
      const state = eventState(event)
      if (state) applyTaskState(state)
      const name = eventSessionName(event)
      if (name) applySessionName(name)
    },
    [applySessionName, applyTaskState],
  )

  const replayTask = useCallback(
    (task: TaskSummary) => {
      replayAbort.current?.abort()
      const controller = new AbortController()
      replayAbort.current = controller
      setStreamError(undefined)
      setTimeline([])

      void streamTaskEvents(
        settings,
        task.task_ref,
        (record) => appendEvent(`event-${record.seq}`, record.event),
        controller.signal,
      ).catch((error: unknown) => {
        if (!isAbortError(error)) {
          setStreamError(error instanceof Error ? error.message : "Task replay failed")
        }
      })
    },
    [appendEvent, settings],
  )

  const openTask = (task: TaskSummary) => {
    stopStreams()
    setBusy(false)
    setSelectedAgentId(task.agent_id)
    setSelectedTask(task)
    setSessionName(task.session_name ?? task.text)
    replayTask(task)
  }

  const newTask = (agent: AgentInfo) => {
    stopStreams()
    setBusy(false)
    setStreamError(undefined)
    setSelectedTask(undefined)
    setSessionName(undefined)
    setSelectedAgentId(agent.id)
    setTimeline([])
    setDraftId((current) => current + 1)
  }

  const submit = async (text: string) => {
    if (!selectedAgent || busy) return
    if (
      selectedTask &&
      !["awaiting_input", "completed", "failed"].includes(selectedTask.state)
    ) return

    replayAbort.current?.abort()
    setStreamError(undefined)
    setBusy(true)
    if (!selectedTask) setSessionName(text)
    detachingRun.current = false
    let httpAgent: HttpAgent | undefined

    try {
      const userId = randomUUID()

      let returnedTaskRef: string | undefined
      const taskRef = selectedTask?.task_ref
      httpAgent = new HttpAgent({
        agentId: selectedAgent.id,
        headers: { Authorization: `Bearer ${settings.token}` },
        initialMessages: [{ id: userId, role: "user", content: text }],
        threadId: selectedTask?.thread_id ?? randomUUID(),
        url: endpoint(settings, `/v1/agents/${encodeURIComponent(selectedAgent.hostname)}/agui`),
      })
      activeAgent.current = httpAgent

      await httpAgent.runAgent(
        taskRef ? { forwardedProps: { task_ref: taskRef } } : undefined,
        {
          onEvent: ({ event }: { event: BaseEvent }) => {
            const hearthEvent = event as unknown as HearthEvent
            liveEventId.current += 1
            appendEvent(`live-${liveEventId.current}`, hearthEvent)
            returnedTaskRef = eventTaskRef(hearthEvent) ?? returnedTaskRef
          },
        },
      )

      const nextRef = returnedTaskRef ?? taskRef
      if (nextRef) {
        const status = await getTask(settings, nextRef)
        const hydrated: TaskSummary = {
          ...status,
          agent_id: selectedAgent.id,
          agent_hostname: selectedAgent.hostname,
          task_ref: nextRef,
        }
        setSelectedTask(hydrated)
        setSessionName(hydrated.session_name ?? hydrated.text)
      }
      await refresh(settings, true)
    } catch (error) {
      if (!detachingRun.current && !isAbortError(error)) {
        setStreamError(error instanceof Error ? error.message : "The agent run failed")
      }
    } finally {
      if (httpAgent && activeAgent.current === httpAgent) activeAgent.current = undefined
      detachingRun.current = false
      setBusy(false)
    }
  }

  const stopRun = () => {
    detachingRun.current = true
    activeAgent.current?.abortRun()
    activeAgent.current = undefined
    setBusy(false)
    setStreamError("Detached from the live run. The durable task may still be running.")
    void refresh(settings, true)
  }

  const handleCancel = async () => {
    if (!selectedTask) return
    if (!window.confirm("Cancel this task? The agent's active run will be stopped.")) return
    try {
      await cancelTask(settings, selectedTask.task_ref)
      stopStreams()
      applyTaskState("canceled")
      await refresh(settings, true)
    } catch (error) {
      setStreamError(error instanceof Error ? error.message : "Could not cancel the task")
    }
  }

  const disconnect = () => {
    stopStreams()
    sessionStorage.removeItem("hearth.token")
    setSettings((current) => ({ ...current, token: "" }))
    setConnected(false)
    setAgents([])
    setTasks([])
    setSelectedTask(undefined)
    setSessionName(undefined)
    setSelectedAgentId(undefined)
    setTimeline([])
  }

  useEffect(() => {
    if (!connected) return
    const interval = window.setInterval(() => {
      void refresh(settings, true).catch(() => undefined)
    }, 10_000)
    return () => window.clearInterval(interval)
  }, [connected, refresh, settings])

  useEffect(() => () => stopStreams(), [stopStreams])

  if (!connected) {
    return (
      <ConnectionPanel
        connecting={connecting}
        defaults={settings}
        error={connectionError}
        onConnect={connect}
      />
    )
  }

  return (
    <div className="dark grid h-svh min-h-0 grid-rows-[minmax(12rem,32vh)_1fr] overflow-hidden bg-neutral-950 md:grid-cols-[19rem_minmax(0,1fr)] md:grid-rows-1">
      <FleetSidebar
        agents={agents}
        onDisconnect={disconnect}
        onNewTask={newTask}
        onOpenTask={openTask}
        onRefresh={() => void refresh().catch((error: unknown) => setStreamError(error instanceof Error ? error.message : "Refresh failed"))}
        refreshing={refreshing}
        selectedAgentId={selectedAgentId}
        selectedTaskId={selectedTask?.task_id}
        tasks={tasks}
      />
      <AgentWorkspace
        agent={selectedAgent}
        busy={busy}
        composerKey={`${selectedTask?.task_id ?? "new"}-${selectedAgentId ?? "none"}-${draftId}`}
        onCancel={() => void handleCancel()}
        onNewTask={() => selectedAgent && newTask(selectedAgent)}
        onQuickResponse={(text) => void submit(text)}
        onRetryReplay={() => selectedTask && replayTask(selectedTask)}
        onStop={stopRun}
        onSubmit={submit}
        streamError={streamError}
        sessionName={displayedSessionName}
        task={selectedTask}
        transcript={transcript}
      />
    </div>
  )
}

export default App
