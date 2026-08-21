<div align="center">

<img src="docs/img/logo.svg" alt="chuanyun" width="280">

**Turn a port on your laptop into a fixed public HTTPS address — on your own server, under your own domain.**

[English](README.md) · [中文](README.zh-CN.md)

[![CI](../../actions/workflows/ci.yml/badge.svg)](../../actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)
[![Made with Slint](https://img.shields.io/badge/Made%20with-Slint-2379f4)](https://slint.dev)
[![npm](https://img.shields.io/npm/v/vite-plugin-chuanyun?label=vite-plugin-chuanyun)](https://www.npmjs.com/package/vite-plugin-chuanyun)

</div>

## What it is

Chuanyun (穿云, "through the clouds") is an intranet tunnelling tool. A desktop client maps
a local port onto a subdomain of **your own** server, so anything outside — a payment
provider's webhook, a customer's browser, a colleague debugging against your machine —
can reach it.

It solves the same problem as frp, with one different assumption: **somebody already set up
the server for you.** Your teammates install the app, paste one credential, and they're
done. No proxy types, no vhost ports, no `subdomain_host`, no config file to maintain.

```
webhook sender ──HTTPS──▶ nginx:443 ──▶ chuanyun server ══tunnel══▶ your laptop ──▶ 127.0.0.1:8082
```

> **A note on language.** This started as an internal tool at a Chinese company, and it
> shows: the desktop UI, the CLI output, and the code comments are all in Chinese. The
> protocol, the config file, and the HTTP API are language-neutral. If you don't read
> Chinese, the deploy and usage guides linked below are in English, but the running
> software will not be.

## Why it exists

Debugging WeChat webhooks, mini-program callbacks, and payment notifications requires a
public HTTPS address on a **domain registered with the Chinese authorities** (ICP filing).
A local server can't receive those callbacks at all. The existing options didn't fit:

- **frp** does everything, but every user hand-writes a config file and there is no
  official GUI. Authentication is one shared global token — you can't tell who is who, and
  you can't revoke one person without rotating it for everyone.
- **ngrok and friends** are pleasant to use, but the traffic goes through a third party,
  and on the free tier the subdomain is random. WeChat wants a specific subdomain typed
  into its console; a new one every restart means reconfiguring every time.

If you already have a server and a registered domain, chuanyun reduces this to: install,
log in, open a tunnel.

## Install

### Server (Linux, x86_64)

```bash
curl -fsSL https://github.com/xsxs89757/chuanyun/releases/latest/download/install-server.sh \
  | sudo sh -s -- --domain t.example.com
```

Downloads the latest release, verifies its checksum, installs a systemd service, and prints
the four things left to do: wildcard DNS, one firewall port, an nginx server block, and the
first credential. Run it again later and it upgrades in place, keeping your config and data.

Not comfortable piping a script into a root shell? Read it first — it's one file,
[`scripts/install-server.sh`](scripts/install-server.sh) — or follow the
[manual steps](docs/deploy.md#manual-install).

### Desktop client (Windows, macOS)

Download from the [latest release](../../releases/latest). The installers are **not
code-signed**, so the first launch needs one extra step per machine:

- **macOS** — double-clicking shows "Apple could not verify...". Click **Done** (*not*
  "Move to Trash"), then open System Settings → Privacy & Security, scroll to the bottom,
  and click **Open Anyway**. Or from a terminal:
  `xattr -dr com.apple.quarantine /Applications/穿云.app`
  (The old right-click → Open trick stopped working in macOS 15.)
- **Windows** — on the SmartScreen prompt: "More info" → "Run anyway".

## Try it without a server

```bash
git clone https://github.com/xsxs89757/chuanyun.git && cd chuanyun
./scripts/demo.sh
```

Brings the whole chain up on your own machine — no domain, no nginx, no public server.

## What you get

- **Stable subdomains** — `{user}-{tunnel}.t.example.com`, unchanged across restarts, safe
  to paste into a webhook console
- **No config files** — the server address can be compiled into the installer (see
  [brand injection](brand/README.md)); a teammate needs one credential and nothing else
- **Per-person credentials** — issue, revoke, audit. Someone leaves, you revoke their
  credential and their connection drops. Not a token everybody shares.
- **Reconnects by itself** — flaky wifi, server restart, closing the laptop lid: the tunnel
  comes back on its own and the address doesn't change
- **A local API** — scripts can register ports on the fly, and application code can ask one
  endpoint for "the public URL if a tunnel is up, `127.0.0.1:port` if not", so the same code
  works in both situations
- **Inspect and replay requests** — every request through a tunnel is kept locally and can
  be re-sent byte-for-byte. Payment webhooks fire a handful of times and then never again;
  replay is the only way to debug them properly.
- **Borrow a colleague's service** — the reverse direction: map someone else's tunnel onto a
  local port, and keep writing `127.0.0.1` in your code
- **A pure-Rust desktop app** — no system WebView, one binary, same on Windows and macOS
- **A deliberately small protocol** — HTTP(S) and TCP over TLS. No UDP, no KCP, no NAT
  hole-punching. A small surface is a maintainable one.

## Documentation

| | |
|---|---|
| [Deploy the server](docs/deploy.md) | DNS, nginx, credentials, troubleshooting |
| [Using it](docs/usage.md) | Tunnels, webhooks, project integration, the local API |
| [Brand injection](brand/README.md) | Ship an internal build your teammates can just log into *(Chinese)* |
| [Vite plugin](integrations/vite-plugin-chuanyun/README.md) | One line in a frontend project |
| [Releasing](docs/发布.md) | Tags, npm publishing, update checks *(Chinese)* |
| [Contributing](CONTRIBUTING.md) | Layout, constraints, how tests are written *(Chinese)* |

## Status

Feature-complete for what it set out to do: HTTP(S) and TCP tunnels, desktop clients for
Windows and macOS, automatic reconnection, request inspection and replay, access passwords,
borrowing a colleague's service, the local API, the Vite plugin, user and credential
management, audit logging.

Not built: a web admin console, peer-to-peer links that never touch the public internet,
QUIC transport.

## Repository layout

| Directory | Contents |
|---|---|
| `crates/cy-proto` | Protocol: control messages, error codes, naming rules. Does no IO. |
| `crates/cy-core` | Client engine: connection, reconnect, tunnels, local API. UI-agnostic. |
| `crates/cy-server` | Server: control channel, HTTP ingress, admin interface |
| `crates/cy-desktop` | Desktop client (Slint) — a thin shell over `cy-core` |
| `crates/cy-e2e` | End-to-end tests |
| `integrations/` | Vite plugin |

`cy-core` and `cy-server` never depend on each other; they only share `cy-proto`.

## License

[Apache-2.0](LICENSE).

The UI is built with [Slint](https://slint.dev). We distribute the desktop app under Slint's
Royalty-Free license, with the attribution shown on the app's About page. If you build from
source you may instead choose Slint's GPLv3 or a commercial license. See
[THIRD-PARTY.md](THIRD-PARTY.md).
