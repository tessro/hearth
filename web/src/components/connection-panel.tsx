import { useState, type FormEvent } from "react"
import { ActivityIcon, FlameIcon, KeyRoundIcon, ServerIcon } from "lucide-react"

import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from "@/components/ui/card"
import { Input } from "@/components/ui/input"
import { Label } from "@/components/ui/label"
import type { ConnectionSettings } from "@/lib/hearth-api"

interface ConnectionPanelProps {
  defaults: ConnectionSettings
  error?: string
  connecting: boolean
  onConnect: (settings: ConnectionSettings) => Promise<void>
}

export function ConnectionPanel({
  defaults,
  error,
  connecting,
  onConnect,
}: ConnectionPanelProps) {
  const [baseUrl, setBaseUrl] = useState(defaults.baseUrl)
  const [token, setToken] = useState(defaults.token)

  const handleSubmit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    void onConnect({ baseUrl, token })
  }

  return (
    <main className="relative flex min-h-svh items-center justify-center overflow-hidden bg-neutral-950 px-5 py-12 text-neutral-100">
      <div className="pointer-events-none absolute inset-0 bg-[radial-gradient(circle_at_50%_20%,rgba(245,158,11,0.13),transparent_34%),linear-gradient(to_bottom,transparent,rgba(0,0,0,0.5))]" />
      <div className="relative grid w-full max-w-5xl items-center gap-12 lg:grid-cols-[1.05fr_0.95fr]">
        <section className="space-y-8">
          <div className="flex items-center gap-3">
            <div className="flex size-10 items-center justify-center rounded-2xl border border-amber-400/20 bg-amber-400/10 text-amber-300 shadow-lg shadow-amber-950/30">
              <FlameIcon className="size-5" />
            </div>
            <span className="text-sm font-semibold tracking-[0.2em] text-neutral-300 uppercase">
              Hearth
            </span>
          </div>
          <div className="max-w-xl space-y-4">
            <h1 className="text-balance text-4xl font-semibold tracking-tight sm:text-5xl">
              Your agent fleet,
              <span className="block text-amber-300">gathered around one fire.</span>
            </h1>
            <p className="max-w-lg text-pretty text-base leading-7 text-neutral-400 sm:text-lg">
              Start durable tasks, follow every turn, review tool activity, and answer approvals
              without opening an SSH session.
            </p>
          </div>
          <div className="flex flex-wrap gap-2">
            <Badge className="border-neutral-800 bg-neutral-900/80 text-neutral-300" variant="outline">
              <ActivityIcon data-icon="inline-start" /> Live AG-UI streams
            </Badge>
            <Badge className="border-neutral-800 bg-neutral-900/80 text-neutral-300" variant="outline">
              Durable replay
            </Badge>
            <Badge className="border-neutral-800 bg-neutral-900/80 text-neutral-300" variant="outline">
              Human approvals
            </Badge>
          </div>
        </section>

        <Card className="border-neutral-800 bg-neutral-900/85 text-neutral-100 shadow-2xl shadow-black/40 backdrop-blur-xl">
          <CardHeader className="border-b border-neutral-800">
            <CardTitle className="flex items-center gap-2 text-base">
              <ServerIcon className="size-4 text-amber-300" /> Connect to agentd
            </CardTitle>
            <CardDescription className="text-neutral-400">
              Credentials stay in this browser tab. Use the same bearer token configured on the
              host daemon.
            </CardDescription>
          </CardHeader>
          <CardContent>
            <form className="space-y-5" onSubmit={handleSubmit}>
              <div className="space-y-2">
                <Label className="text-neutral-300" htmlFor="base-url">
                  API URL
                </Label>
                <Input
                  autoCapitalize="none"
                  autoComplete="url"
                  className="border-neutral-700 bg-neutral-950/70 text-neutral-100 placeholder:text-neutral-600"
                  id="base-url"
                  onChange={(event) => setBaseUrl(event.target.value)}
                  placeholder="http://127.0.0.1:8787"
                  spellCheck={false}
                  value={baseUrl}
                />
                <p className="text-xs text-neutral-500">
                  Leave blank when the web app is reverse-proxied to agentd.
                </p>
              </div>
              <div className="space-y-2">
                <Label className="text-neutral-300" htmlFor="token">
                  Bearer token
                </Label>
                <div className="relative">
                  <KeyRoundIcon className="pointer-events-none absolute top-1/2 left-3 size-4 -translate-y-1/2 text-neutral-500" />
                  <Input
                    autoComplete="current-password"
                    className="border-neutral-700 bg-neutral-950/70 pl-9 text-neutral-100 placeholder:text-neutral-600"
                    id="token"
                    onChange={(event) => setToken(event.target.value)}
                    placeholder="Paste your agent token"
                    required
                    type="password"
                    value={token}
                  />
                </div>
              </div>
              {error ? (
                <div className="rounded-lg border border-red-900/70 bg-red-950/40 px-3 py-2 text-sm text-red-300">
                  {error}
                </div>
              ) : null}
              <Button
                className="h-9 w-full bg-amber-300 text-neutral-950 hover:bg-amber-200"
                disabled={connecting || !token.trim()}
                type="submit"
              >
                {connecting ? "Connecting…" : "Open console"}
              </Button>
            </form>
          </CardContent>
        </Card>
      </div>
    </main>
  )
}
