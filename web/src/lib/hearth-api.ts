export type TaskState =
  | "queued"
  | "running"
  | "awaiting_input"
  | "completed"
  | "failed"
  | "canceled"

export interface AgentInfo {
  name: string
  running: boolean
  ready: boolean
  is_agent_in_charge: boolean
  adapters: string[]
  task_count: number
}

export interface RunRecord {
  run_id: string
  outcome?: "finished" | "error" | "interrupted"
  started_at: string
  ended_at?: string
}

export interface TaskSummary {
  task_id: string
  task_ref: string
  thread_id: string
  agent: string
  agent_vm: string
  text: string
  session_name?: string
  state: TaskState
  incarnation: string
  last_seq: number
  created_at: string
  updated_at: string
  result?: unknown
  pending_input?: unknown
  failure?: string
  runs: RunRecord[]
}

export interface HearthEvent {
  type: string
  [key: string]: unknown
}

export interface HearthEventRecord {
  seq: number
  run_id: string
  event: HearthEvent
}

export interface ConnectionSettings {
  baseUrl: string
  token: string
}

const normalizeBaseUrl = (value: string) => value.trim().replace(/\/+$/, "")

export const endpoint = (settings: ConnectionSettings, path: string) =>
  `${normalizeBaseUrl(settings.baseUrl)}${path}`

const errorMessage = async (response: Response) => {
  const fallback = `${response.status} ${response.statusText}`.trim()
  try {
    const body = (await response.json()) as { error?: string }
    return body.error || fallback
  } catch {
    return fallback
  }
}

export const request = async <T>(
  settings: ConnectionSettings,
  path: string,
  init?: RequestInit,
): Promise<T> => {
  const response = await fetch(endpoint(settings, path), {
    ...init,
    headers: {
      Accept: "application/json",
      Authorization: `Bearer ${settings.token}`,
      ...init?.headers,
    },
  })

  if (!response.ok) {
    throw new Error(await errorMessage(response))
  }

  return (await response.json()) as T
}

export const listAgents = async (settings: ConnectionSettings) => {
  const response = await request<{ agents: AgentInfo[] }>(settings, "/v1/agents")
  return response.agents
}

export const listTasks = async (settings: ConnectionSettings) => {
  const response = await request<{ tasks: TaskSummary[] }>(settings, "/v1/tasks")
  const missingRef = response.tasks.find(
    (task) => typeof task.task_ref !== "string" || task.task_ref.length === 0,
  )
  if (missingRef) {
    throw new Error(
      "agentd is missing signed task references; rebuild and restart hearth-agentd",
    )
  }
  return response.tasks.sort((a, b) => b.updated_at.localeCompare(a.updated_at))
}

export const getTask = (settings: ConnectionSettings, taskRef: string) =>
  request<TaskSummary>(settings, `/v1/tasks/${encodeURIComponent(taskRef)}`)

export const cancelTask = (settings: ConnectionSettings, taskRef: string) =>
  request<TaskSummary>(
    settings,
    `/v1/tasks/${encodeURIComponent(taskRef)}/cancel`,
    { method: "POST" },
  )

const parseSseBlock = (block: string): unknown | undefined => {
  const data = block
    .split(/\r?\n/)
    .filter((line) => line.startsWith("data:"))
    .map((line) => line.slice(5).trimStart())
    .join("\n")

  if (!data) return undefined

  try {
    return JSON.parse(data)
  } catch {
    return undefined
  }
}

export const streamTaskEvents = async (
  settings: ConnectionSettings,
  taskRef: string,
  onRecord: (record: HearthEventRecord) => void,
  signal: AbortSignal,
) => {
  const response = await fetch(
    endpoint(settings, `/v1/tasks/${encodeURIComponent(taskRef)}/events?cursor=`),
    {
      headers: {
        Accept: "text/event-stream",
        Authorization: `Bearer ${settings.token}`,
      },
      signal,
    },
  )

  if (!response.ok) {
    throw new Error(await errorMessage(response))
  }
  if (!response.body) {
    throw new Error("The task event stream returned no body")
  }

  const reader = response.body.getReader()
  const decoder = new TextDecoder()
  let buffer = ""

  while (true) {
    const { value, done } = await reader.read()
    buffer += decoder.decode(value, { stream: !done }).replaceAll("\r\n", "\n")

    let boundary = buffer.indexOf("\n\n")
    while (boundary >= 0) {
      const block = buffer.slice(0, boundary)
      buffer = buffer.slice(boundary + 2)
      const parsed = parseSseBlock(block)
      if (
        parsed &&
        typeof parsed === "object" &&
        "event" in parsed &&
        "seq" in parsed &&
        "run_id" in parsed
      ) {
        onRecord(parsed as HearthEventRecord)
      }
      boundary = buffer.indexOf("\n\n")
    }

    if (done) break
  }
}

export const isAbortError = (error: unknown) =>
  error instanceof DOMException && error.name === "AbortError"
