# Hearth web

The operator console for Hearth's agent plane. It uses React 19, TypeScript 7,
Vite 8.1, Tailwind CSS 4, shadcn/ui on Base UI, Vercel AI Elements, and the
official `@ag-ui/client` `HttpAgent`.

## Development

From the repository root, enter the devenv shell and start the app:

```sh
devenv shell
cd web
pnpm install
pnpm dev
```

Vite listens only on `127.0.0.1:5173`. Put a TLS reverse proxy in front of it,
open that HTTPS origin, and leave the API URL blank on the connection screen.
Vite proxies `/v1` locally to `http://127.0.0.1:8787`, so neither Vite nor
agentd needs a network-facing bind.

Override the development proxy target when agentd is elsewhere:

```sh
VITE_HEARTH_DEV_PROXY_TARGET=http://host:8787 pnpm dev
```

For a separately hosted production build, set `VITE_HEARTH_API_URL` at build
time or enter the agentd URL in the connection screen. The origin serving the
web app must be present in agentd's `HEARTH_AGENT_CORS_ORIGINS` allowlist.

The bearer token is kept in `sessionStorage`, while the non-secret API URL is
remembered in `localStorage`.

## Verification

```sh
pnpm typecheck
pnpm lint
pnpm build
```
