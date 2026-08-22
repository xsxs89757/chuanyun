# Using it

[English](usage.md) · [中文](使用.md)

## After installing

Open chuanyun, paste the credential your admin gave you, and click log in.

If you were given an internal build, the server address and certificate fingerprint are
already baked in — you only need the credential.

**If the system blocks the first launch**: the app isn't code-signed (a certificate costs a
few hundred dollars a year, which isn't worth it for an internal tool).

- **macOS** — double-clicking shows "Apple could not verify...". Click **Done** (*not*
  "Move to Trash"), then open System Settings → Privacy & Security, scroll to the bottom,
  and click **Open Anyway**. Or from a terminal:
  `xattr -dr com.apple.quarantine /Applications/穿云.app`
  (The old right-click → Open trick stopped working in macOS 15.)
- **Windows** — on the SmartScreen prompt: "More info" → "Run anyway".

Once per machine.

## Opening a tunnel

Say you have a service running locally on port 8082.

Click "new tunnel", name it `wx`, port `8082`. A few seconds later you get:

```
https://yourname-wx.t.yourcompany.com
```

That address **doesn't change** — not when you restart the client, not when you reboot. Safe
to paste into a webhook console.

Anything hitting that address reaches port 8082 on your machine.

## Debugging a webhook

1. Start your service locally
2. Open a tunnel pointing at it
3. Paste the tunnel address into the provider's callback URL field
4. Set a breakpoint and trigger the callback

The callback lands on your machine and you can step through it. The address is stable, so
tomorrow you don't have to set it up again.

## Wiring it into a project

### In the UI

Click "new tunnel", give it a name and a port. Tunnels are remembered and restored when the
app reopens — this covers most cases.

### Scripted

Chuanyun exposes an HTTP API on `127.0.0.1:7075`. One line in your start script registers
the ports:

```bash
curl -s -X POST localhost:7075/api/tunnels -H 'Content-Type: application/json' \
     -d '[{"port":8082,"name":"api"},{"port":5666,"name":"web"}]'
```

Idempotent by name — running the script repeatedly won't pile up duplicate tunnels, and it
**won't strip a password you set in the client** (re-registering without `auth` keeps the
existing one). To put a password on one, add a field:

```bash
curl -s -X POST localhost:7075/api/tunnels -H 'Content-Type: application/json' \
     -d '{"port":8082,"name":"api","auth":"demo:s3cret"}'
```

A misspelled field is an error, not silently ignored.

### Let the callback URL follow the environment

This is the one worth adopting. Don't hard-code the address; ask chuanyun for it:

```bash
# tunnel up   → the public address
# tunnel down → http://127.0.0.1:8082
curl -s 'localhost:7075/api/resolve?port=8082&plain=1'
```

Drop it in an environment variable and your application code doesn't change at all:

```bash
export WX_CALLBACK_BASE=$(curl -s 'localhost:7075/api/resolve?port=8082&plain=1')
npm run dev
```

If chuanyun isn't running the command fails, so give it a fallback:

```bash
BASE=$(curl -sf 'localhost:7075/api/resolve?port=8082&plain=1' || echo "http://127.0.0.1:8082")
```

## Local API reference

| Endpoint | What it does |
|---|---|
| `GET /api/status` | Connected or not, and the domain suffix |
| `GET /api/tunnels` | All current tunnels |
| `POST /api/tunnels` | Register a tunnel; an object or an array. Optional `auth` (password), `domain` (custom domain) |
| `DELETE /api/tunnels/{name}` | Unregister |
| `PATCH /api/tunnels/{name}` | Change the password: `{"auth":"user:password"}`; `""` removes it. Address unchanged |
| `GET /api/resolve?port=N` | The address this port is reachable at; add `&plain=1` for plain text |
| `GET /api/requests` | Recorded requests; `?tunnel=name` to filter |
| `GET /api/requests/{id}` | One request in full |
| `POST /api/requests/{id}/replay` | Replay it |
| `DELETE /api/requests` | Clear the log |
| `GET /api/connects` | Current inbound links to colleagues' services |
| `POST /api/connects` | Create one: `{"local_port":8082,"from":"zhangsan-api"}` |
| `DELETE /api/connects/{port}` | Remove one |

It binds to loopback only, and it **rejects requests that look like they came from a
browser** — the threat being a malicious web page quietly probing your local services. So
these endpoints work from the shell and from scripts, but not from `fetch()` in a page.

## Putting a password on a tunnel

Demoing to a client, or leaving a tunnel up all day? Add a door.

When **creating** the tunnel, fill in the "access password" field as `username:password`
(e.g. `demo:s3cret`). Browsers will prompt for it; requests that fail the check never reach
your local service.

A protected tunnel's card is marked **"password set"** — glance at it before sending the
address to someone, so they aren't met with a prompt you forgot about. The password is saved
with the tunnel and survives reconnects and app restarts.

To change or remove it, click **"change password"** on the tunnel's card ("set password"
if it has none), enter the new one and save; saving it empty removes the password. The address
stays the same; the new password takes effect immediately and the old one stops working.

## Databases and SSH (TCP tunnels)

For anything that isn't HTTP, use a TCP tunnel: choose TCP when creating it and chuanyun
allocates a public port.

```
127.0.0.1:3306  →  server.example.com:20017
```

Use that as the host in your database client. Note that each TCP tunnel holds one public port
from a limited pool — close them when you're done.

## Inspecting and replaying requests

The "requests" page shows everything that came through a tunnel: method, path, status,
duration.

The useful part is **replay**. A payment webhook fires a handful of times and then never
again. Click replay and the exact same payload — same signature, same timestamp — goes
through once more. As many times as you need.

Records live in memory on your own machine and are gone when you quit. Cookie and
Authorization headers are redacted, so a screenshot pasted into a group chat won't leak
anything.

## Borrowing a colleague's service

You're working on the frontend and don't want to run the whole backend locally — you'd
rather point at your colleague's instance, which already has test data.

On the "connect" page, enter the source (say `zhangsan-api`) and the local port to map it to
(say `8082`). Now `127.0.0.1:8082` on your machine goes to zhangsan's service.

**The point is that your config doesn't change**: the frontend proxy, `.env`, and your code
all keep saying `127.0.0.1:8082`. Switching between "my own" and "zhangsan's" is just
toggling that link.

If you're also running something on 8082, the link will tell you the port is taken — stop
yours or pick a different port.

## One line in a Vite project

```bash
npm i -D vite-plugin-chuanyun     # pnpm add -D / yarn add -D also work
```

```ts
import chuanyun from 'vite-plugin-chuanyun'
export default defineConfig({ plugins: [chuanyun({ name: 'admin' })] })
```

The dev server comes up with a tunnel already open and the address printed next to Local. It
also handles two annoyances for you: Vite blocks unknown hosts by default
(`Blocked request`), and the port drifts to 5174 when 5173 is taken.

See the [plugin README](../integrations/vite-plugin-chuanyun/README.md).

## Common questions

**The address says "no tunnel running"**

The tunnel is switched off, or the address is mistyped. Check the toggle in the client.

**The tunnel is on, but I get "nothing answered on the other end"**

Your local service isn't running, or the port is wrong. The client shows the specific reason
under that tunnel.

**My dev server says "Blocked request: this host is not allowed"**

Vite only allows localhost by default. The [plugin](../integrations/vite-plugin-chuanyun/README.md)
handles it, or allow it by hand:

```ts
server: { allowedHosts: ['.t.yourcompany.com'] }
```

**Does the tunnel survive closing the laptop lid?**

Yes. The client reconnects and the tunnels come back, at the same addresses.

**Does closing the window disconnect me?**

No — that just hides it in the tray. Tunnels keep running. Use "quit" in the tray menu to
actually exit.

**Where is the config stored?**

```bash
chuanyun --print-state-path
```

## One thing to keep in mind

A tunnel address is **reachable from the public internet**. While it's on, anyone who knows
the address can reach that port on your machine.

- Don't expose a service wired to a production database
- Close the tunnel when the demo is over
- When in doubt, off is better than on
