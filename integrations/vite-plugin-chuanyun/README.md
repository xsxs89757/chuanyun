# vite-plugin-chuanyun

[English](README.md) · [中文](README.zh-CN.md)

Expose a Vite dev server through a [chuanyun](https://github.com/xsxs89757/chuanyun) tunnel,
automatically.

## Install

```bash
npm i -D vite-plugin-chuanyun
```

```bash
pnpm add -D vite-plugin-chuanyun
```

```bash
yarn add -D vite-plugin-chuanyun
```

<details>
<summary>Installing without npm</summary>

pnpm can install straight from a subdirectory of the repo:

```bash
pnpm add -D "github:xsxs89757/chuanyun#path:/integrations/vite-plugin-chuanyun"
```

npm and yarn don't support subdirectories, so build a tarball first:

```bash
git clone https://github.com/xsxs89757/chuanyun.git
cd chuanyun/integrations/vite-plugin-chuanyun
npm install && npm pack        # produces vite-plugin-chuanyun-x.y.z.tgz

cd /your/frontend/project
npm i -D /path/to/vite-plugin-chuanyun-x.y.z.tgz
```

</details>

## Usage

```ts
// vite.config.ts
import { defineConfig } from 'vite'
import chuanyun from 'vite-plugin-chuanyun'

export default defineConfig({
  plugins: [chuanyun({ name: 'admin' })],
})
```

Start the dev server and you get one extra line:

```
  ➜  Local:   http://localhost:5173/
  ➜  穿云:    https://zhangsan-admin.t.example.com
```

That address is reachable from the public internet and stays the same across restarts — paste
it into a webhook console, send it to a colleague, open it on your phone.

## What it does for you

**Allows the tunnel host.** Vite blocks Hosts it doesn't recognise, so going through a tunnel
you'd hit `Blocked request: this host is not allowed`. The plugin adds the tunnel domain to
`allowedHosts` for you (anything you configured yourself is preserved, not overwritten).

**Uses the real port, not the configured one.** When 5173 is taken Vite moves to 5174, and a
tunnel pinned to the configured port would point at nothing. The plugin waits until the dev
server is actually listening before registering.

**Unregisters on exit.** Shut down the dev server and the tunnel goes down with it.

## Options

| Option | Default | Meaning |
|---|---|---|
| `name` | the project name from package.json | Tunnel name; becomes part of the address. `@company/admin-panel` becomes `admin-panel` |
| `apiPort` | `7075` | Port of the local chuanyun API |
| `auth` | none | Access password (`user:password`) to put a door on the tunnel |
| `enabled` | `true` | Set to `false` to skip it, e.g. in CI |

Per-environment:

```ts
plugins: [chuanyun({ enabled: !process.env.CI })]
```

## Requirements

The [chuanyun desktop client](https://github.com/xsxs89757/chuanyun) must be installed and
logged in on the same machine.

**If it isn't, nothing breaks.** The plugin prints one line and gets out of the way; the dev
server starts as usual and `localhost` works as usual. Not installed, not logged in,
connection refused, timed out — every one of those degrades silently. An optional convenience
should never be able to stop local development.

## HMR

Modern Vite opens its HMR WebSocket against the page origin, which through a tunnel is
`wss://`, and chuanyun passes it through — no configuration needed. Older versions may need:

```ts
server: { hmr: { clientPort: 443 } }
```

## License

Apache-2.0
