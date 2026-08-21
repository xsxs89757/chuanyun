# Deploying the server

[English](deploy.md) · [中文](部署.md)

A one-time job. Once it's done, your teammates install the desktop app and are ready to go.

## What you need

- A Linux server with a public IP (an existing one is fine; prebuilt binaries are x86_64 only)
- A domain where you can add a wildcard DNS record
- A wildcard TLS certificate for that domain

> In mainland China, webhook URLs for WeChat official accounts, mini-programs, and payments
> must use an ICP-filed domain. If your domain is already serving traffic there, it qualifies.

Decide up front which subdomain the tunnels will live under — say `t.example.com`.
**Use a dedicated subdomain, not your apex domain**: tunnel traffic stays separated from your
production site, problems don't spill over, and cookies don't leak between them.

## One command

```bash
curl -fsSL https://github.com/xsxs89757/chuanyun/releases/latest/download/install-server.sh \
  | sudo sh -s -- --domain t.example.com
```

The script downloads the latest release and verifies its checksum, installs the binary to
`/usr/local/bin`, creates a `chuanyun` system user, writes `/etc/chuanyun/server.toml`,
registers and starts a systemd service, and then prints the four remaining steps.

Rather not pipe a script into a root shell? Read it first — it's a single file,
[`scripts/install-server.sh`](../scripts/install-server.sh) — or follow the
[manual steps](#manual-install) below.

Other options:

```bash
--version v0.1.0      # install a specific version (default: latest)
--control-port 7100   # control channel, what clients dial (default: 7000)
--http-port 7080      # HTTP ingress, loopback only, for nginx (default: 7080)
--admin-port 7101     # admin interface, loopback only (default: 7001)
--no-start            # install but don't start
--uninstall           # remove it (data directory is kept)
```

**You will probably need to change the ports.** Servers rarely sit empty — frp
defaults to 7000, and docker frequently holds 7001. The script checks all three
before touching anything, and if one is taken it tells you which port, what holds
it, and which flag to pass, without changing a thing.

## When GitHub is slow or unreachable

From servers in mainland China, `raw.githubusercontent.com` is usually
unreachable and `github.com` itself is intermittent (on one test machine,
downloads ran at roughly 20 KB/s). So:

- The command above uses `github.com/…/releases/latest/download/…`, which
  redirects to `objects.githubusercontent.com` — far more reliable. **Don't**
  substitute the raw.githubusercontent.com URL.
- The script retries three times and allows 600 seconds for the download.

If it still won't come down, install offline: fetch the files somewhere with good
connectivity, copy them over, and point the script at them.

```bash
# On a machine with working GitHub access
VER=0.1.0
curl -fLO https://github.com/xsxs89757/chuanyun/releases/download/v$VER/chuanyun-server-$VER-linux-x86_64.tar.gz
curl -fLO https://github.com/xsxs89757/chuanyun/releases/download/v$VER/SHA256SUMS
curl -fLO https://github.com/xsxs89757/chuanyun/releases/download/v$VER/install-server.sh
scp chuanyun-server-$VER-linux-x86_64.tar.gz SHA256SUMS install-server.sh root@server:/tmp/

# On the server
cd /tmp && CHUANYUN_BASE_URL=file:///tmp sh install-server.sh \
  --domain t.example.com --version v$VER
```

`CHUANYUN_BASE_URL` can point anywhere those files live — a local directory, an
internal HTTP server, object storage. Checksums are still verified.

## After installing: four steps left

The script prints these with your actual IP, port, and certificate fingerprint filled in.
This is the same list, for reference.

### 1. Wildcard DNS

```
*.t.example.com   A   your.server.ip
```

### 2. Open the control-channel port

Clients dial out to this port (7000 by default). That connection carries its own TLS and
**does not go through nginx**.

```bash
sudo ufw allow 7000/tcp
# or: sudo firewall-cmd --add-port=7000/tcp --permanent && sudo firewall-cmd --reload
```

⚠️ **On a cloud VM you must also open it in the provider's security group.** Opening the OS
firewall alone is not enough, and this is the single most common reason clients can't connect.

### 3. Put nginx in front

The server listens on plaintext HTTP bound to loopback (`127.0.0.1:7080`). Port 443 and the
certificates stay with nginx — no port to fight over, and certificate renewal keeps working
exactly as it does today.

A sample config is on the machine at `/etc/chuanyun/nginx.conf.example` (also in the repo at
[`deploy/nginx.conf.example`](../deploy/nginx.conf.example)). Change the domain and
certificate paths. Four things matter:

| Directive | Why |
|---|---|
| `proxy_set_header Host $host` | Subdomain routing depends entirely on the original Host header |
| `Upgrade` / `Connection` headers | Needed for WebSocket |
| `proxy_buffering off` | Otherwise SSE isn't real-time and large responses buffer to disk first |
| `client_max_body_size 200m` | Large uploads |

**baota (宝塔) panel**: put the config in its own file under
`/www/server/panel/vhost/nginx/`. Don't edit the site config the panel generates — it
rewrites that file and your changes disappear.

### Coexisting with an existing frp

Plenty of servers already run frp. The two coexist fine, as long as **chuanyun gets its own
level of subdomain**.

Say frp has `subDomainHost = example.com`, so it owns `xxx.example.com`. Set chuanyun's
`domain_suffix` to `t.example.com` and give it its own server block:

```nginx
server {
    listen 443 ssl;
    server_name *.t.example.com;      # longer than *.example.com, so nginx prefers it
    ssl_certificate     /path/to/t.example.com/fullchain.pem;
    ssl_certificate_key /path/to/t.example.com/privkey.pem;
    location / { proxy_pass http://127.0.0.1:7080; /* headers as above */ }
}
```

nginx matches leading wildcards **longest-first**, so `*.t.example.com` wins over frp's
`*.example.com`. Only that level reaches chuanyun; everything else still goes to frp.

Two things to watch:

- **Ports.** frp defaults to 7000, which is also chuanyun's default control port. Pass
  `--control-port` at install time (the installer checks first and tells you if it clashes).
- **Certificates.** A cert for `*.example.com` does **not** cover `xxx.t.example.com` —
  wildcards only span one level. Issue a separate one for `*.t.example.com`; wildcards
  require DNS-01:

  ```bash
  acme.sh --issue --dns dns_ali -d '*.t.example.com' -d 't.example.com' --server letsencrypt
  acme.sh --install-cert -d '*.t.example.com' --ecc \
      --fullchain-file /path/to/t.example.com/fullchain.pem \
      --key-file       /path/to/t.example.com/privkey.pem \
      --reloadcmd      "nginx -t && nginx -s reload"
  ```

### 4. Issue a credential

```bash
sudo chuanyun-server user add zhangsan
```

The credential is **shown once** — only a hash is stored. If it's lost, issue a new one:

```bash
sudo chuanyun-server user reissue zhangsan
```

The old credential stops working immediately and the person's live connection is dropped;
they log in again with the new one. (`user add` refuses an existing name rather than
overwriting it, so a mistyped name can't knock someone offline.)

Send it to the person along with the **certificate fingerprint**:

```bash
chuanyun-server fingerprint
```

The fingerprint is how the client confirms it's talking to your server and not someone else's.
It isn't a secret — sending it through the same channel as the credential is fine.

## Verify

Have a teammate install the client, paste the credential, and open a tunnel. Then hit that
address from outside your network.

If it works, you're done. If not, see [troubleshooting](#troubleshooting).

## Day-to-day

```bash
chuanyun-server status                # who's online, how many tunnels
chuanyun-server user list             # who has credentials, and expiry
chuanyun-server user add <name>       # add someone
chuanyun-server user reissue <name>   # rotate a credential (lost, or to undo a revoke)
chuanyun-server user revoke <name>    # revoke (drops their live connection immediately)
chuanyun-server kick <name>           # disconnect only; the credential stays valid
chuanyun-server fingerprint           # show the certificate fingerprint again
chuanyun-server domain add <user> <domain>   # register a custom domain

systemctl status chuanyun-server      # is it running
journalctl -u chuanyun-server -f      # logs
```

## Upgrading

Run the same install command again. Config and data are left alone:

```bash
curl -fsSL https://github.com/xsxs89757/chuanyun/releases/latest/download/install-server.sh | sudo sh
```

No `--domain` needed on an upgrade — the config already has it. Tunnels blip for a couple of
seconds while the service restarts, then clients reconnect on their own. Addresses don't change.

## Uninstalling

```bash
curl -fsSL https://github.com/xsxs89757/chuanyun/releases/latest/download/install-server.sh | sudo sh -s -- --uninstall
```

Stops and removes the service and deletes the binary. **Config and data are deliberately
kept** — the data directory holds the self-signed certificate, and deleting it means every
client has to be given a new fingerprint. Remove it by hand once you're sure:

```bash
sudo rm -rf /etc/chuanyun /var/lib/chuanyun && sudo userdel chuanyun
```

## Manual install

If you'd rather not use the script, or the machine has no systemd:

```bash
# 1. Download and verify
VER=0.1.0
curl -fLO https://github.com/xsxs89757/chuanyun/releases/download/v$VER/chuanyun-server-$VER-linux-x86_64.tar.gz
curl -fLO https://github.com/xsxs89757/chuanyun/releases/download/v$VER/SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing
tar xzf chuanyun-server-$VER-linux-x86_64.tar.gz

# 2. Install the binary
sudo install -m 0755 chuanyun-server /usr/local/bin/

# 3. Config
sudo mkdir -p /etc/chuanyun
sudo tee /etc/chuanyun/server.toml > /dev/null <<'TOML'
[http]
domain_suffix = "t.example.com"

[storage]
data_dir = "/var/lib/chuanyun"
TOML

# 4. System user and data directory
sudo useradd --system --no-create-home --shell /usr/sbin/nologin chuanyun
sudo mkdir -p /var/lib/chuanyun
sudo chown chuanyun /var/lib/chuanyun
sudo chmod 0700 /var/lib/chuanyun

# 5. systemd
sudo install -m 0644 chuanyun-server.service /etc/systemd/system/
sudo systemctl enable --now chuanyun-server
```

`domain_suffix` is the only required setting. Everything else has a sensible default:
control channel on `0.0.0.0:7000`, HTTP ingress on `127.0.0.1:7080`, admin interface on
`127.0.0.1:7001`.

Without systemd, run it directly: `chuanyun-server -c /etc/chuanyun/server.toml run`.

Then come back to [the four steps](#after-installing-four-steps-left).

## Troubleshooting

**A client can't connect**

Check in this order:

```bash
nc -vz your.server 7000            # is the port reachable
systemctl status chuanyun-server   # is the service running
chuanyun-server fingerprint        # does it match what the client has
```

Nine times out of ten it's **the cloud security group not allowing port 7000** — `ufw allow`
only covers the OS firewall; the provider's layer is separate.

A wrong fingerprint produces an explicit "certificate fingerprint mismatch" error rather than
a vague failure.

**The tunnel address shows a branded 404 page**

The request reached the server but no tunnel matched. Either the tunnel isn't switched on in
the client, or nginx isn't passing the Host header through
(`proxy_set_header Host $host`) — subdomain routing depends entirely on it.

**The tunnel address won't load, or the certificate is rejected**

```bash
dig zhangsan-test.t.example.com   # has the wildcard record propagated
```

Then check the wildcard certificate actually covers that level — a cert for
`*.t.example.com` does **not** cover `a.b.t.example.com`.

**WeChat rejects the callback URL**

WeChat wants a specific subdomain and does not accept wildcards. Paste the full address shown
in the client, unmodified. The domain must be ICP-filed.

**Works in a desktop browser, fails on old Android or inside WeChat**

Usually a certificate chain problem. Let's Encrypt's newer root isn't trusted on some older
devices — issue with the full chain, or use a different free CA.

**The service won't start**

```bash
journalctl -u chuanyun-server -n 30 --no-pager
```

A malformed config names the offending key. Misspelled keys are rejected rather than silently
ignored — deliberately, because a typo that does nothing is the hardest kind to find.

## A download page for your team

The server ships with a download page. Drop installers into the download directory and
buttons for them appear:

```bash
sudo mkdir -p /var/lib/chuanyun/downloads
sudo cp chuanyun-*.dmg chuanyun-*.msi /var/lib/chuanyun/downloads/
sudo chown -R chuanyun /var/lib/chuanyun/downloads
```

Only `.dmg`, `.msi`, and `.exe` are recognised; anything else in that directory is neither
listed nor downloadable. To use a different directory:

```toml
[admin]
download_dir = "/opt/chuanyun/pkgs"
```

The page lives on the admin interface (`127.0.0.1:7001` by default). **Proxy only the two
download-related paths** — the rest of the admin interface can kick users and list
credentials and audit logs, so don't expose it:

```nginx
# The page itself, and the installers
location ^~ /chuanyun/download { proxy_pass http://127.0.0.1:7001/download; }
# Client update check (optional)
location =  /chuanyun/version  { proxy_pass http://127.0.0.1:7001/api/client/version; }
```

The `^~` matters: installers are served from `/chuanyun/download/<filename>`.

The page explains how to get past Gatekeeper for unsigned installers, and shows the
domain suffix and certificate fingerprint so colleagues can fill them in themselves.
