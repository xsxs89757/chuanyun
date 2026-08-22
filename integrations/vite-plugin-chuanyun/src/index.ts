/**
 * 把 Vite 的 dev server 自动接进穿云隧道。
 *
 * 前端是少数几个值得单独做插件的地方，因为 Vite 有两个绕不开的坎：
 *
 * 1. **dev server 会拦下不认识的 Host。** 经隧道访问时 Host 是隧道域名，
 *    Vite 会回一句 "Blocked request: this host is not allowed"。
 * 2. **端口会漂。** 5173 被占就自动换 5174，写死在配置里的隧道于是指错了地方。
 *
 * 插件解决的就是这两件事：等 dev server 真正起来之后拿**实际端口**去注册，
 * 并把隧道域名加进 allowedHosts。退出时注销。
 *
 * ```ts
 * import chuanyun from 'vite-plugin-chuanyun'
 *
 * export default defineConfig({
 *   plugins: [chuanyun({ name: 'admin' })],
 * })
 * ```
 */

import type { Plugin, ViteDevServer } from 'vite'

export interface ChuanyunOptions {
  /**
   * 隧道名，会变成公网地址的一部分：`{用户}-{name}.{域名后缀}`。
   * 不填就用 package.json 里的项目名。
   */
  name?: string
  /** 穿云客户端的本地接口端口。默认 7075，一般不用改。 */
  apiPort?: number
  /**
   * 关掉插件。用来在 CI 或者其他不该建隧道的环境里跳过，
   * 而不用把 plugins 数组改来改去。
   */
  enabled?: boolean
  /** 访问口令（`用户名:口令`），给隧道加一道门。 */
  auth?: string
}

interface TunnelResult {
  name: string
  ok: boolean
  url?: string
  error?: string
}

interface StatusResult {
  connected: boolean
  domain_suffix: string
  needs_login: boolean
}

const DEFAULT_API_PORT = 7075

/** 本地接口的请求超时。它就在本机，慢说明它没在跑。 */
const TIMEOUT_MS = 3000

export default function chuanyun(options: ChuanyunOptions = {}): Plugin {
  const apiPort = options.apiPort ?? DEFAULT_API_PORT
  const base = `http://127.0.0.1:${apiPort}`
  let registeredName: string | undefined

  return {
    name: 'vite-plugin-chuanyun',
    // 只在 dev 时起作用——build 的产物跟隧道没关系
    apply: 'serve',

    async config(config) {
      if (options.enabled === false) return

      // allowedHosts 要在 server 起来之前配好，所以这里先问一次域名后缀。
      // 问不到也不要紧：那多半是穿云没开着，本来也不会有隧道。
      const status = await getStatus(base)
      if (!status?.domain_suffix) return

      return {
        server: {
          allowedHosts: mergeAllowedHosts(
            config.server?.allowedHosts,
            `.${status.domain_suffix}`,
          ),
        },
      }
    },

    configureServer(server: ViteDevServer) {
      if (options.enabled === false) return

      // 必须等 listening：端口可能和配置里写的不一样（5173 被占就会往后挪），
      // 只有这时候拿到的才是真端口。
      server.httpServer?.once('listening', () => {
        void register(server, base, options).then((name) => {
          registeredName = name
        })
      })

      const cleanup = () => {
        if (!registeredName) return
        // 进程要退了，来不及等异步请求——发一枪不等回应的注销
        unregisterSync(apiPort, registeredName)
        registeredName = undefined
      }
      server.httpServer?.once('close', cleanup)
      process.once('exit', cleanup)
      process.once('SIGINT', () => {
        cleanup()
        process.exit(0)
      })
    },
  }
}

async function register(
  server: ViteDevServer,
  base: string,
  options: ChuanyunOptions,
): Promise<string | undefined> {
  const address = server.httpServer?.address()
  if (!address || typeof address === 'string') return

  const port = address.port
  const name = options.name ?? (await guessName())

  const status = await getStatus(base)
  if (!status) {
    // 穿云没开着不该让 dev server 起不来——大多数时候本地开发根本用不到隧道
    server.config.logger.info(
      dim('  ➜  穿云:   未运行（本次不建隧道）'),
    )
    return
  }
  if (status.needs_login) {
    server.config.logger.warn(yellow('  ➜  穿云:   还没登录，本次不建隧道'))
    return
  }

  const body: Record<string, unknown> = { port, name }
  if (options.auth) body.auth = options.auth

  const results = await post<TunnelResult[]>(`${base}/api/tunnels`, body)
  const result = results?.[0]

  if (!result?.ok) {
    server.config.logger.warn(
      yellow(`  ➜  穿云:   建隧道失败 — ${result?.error ?? '未知原因'}`),
    )
    return
  }

  // 打在 Local / Network 旁边，和 Vite 自己的输出连成一片
  server.config.logger.info(
    `  ${dim('➜')}  ${bold('穿云')}:   ${cyan(result.url ?? '')}`,
  )
  return result.name
}

async function getStatus(base: string): Promise<StatusResult | undefined> {
  return get<StatusResult>(`${base}/api/status`)
}

/** package.json 里的名字，去掉 scope 并转成合法的隧道名。 */
async function guessName(): Promise<string> {
  try {
    const { readFile } = await import('node:fs/promises')
    const raw = await readFile('package.json', 'utf8')
    const pkg = JSON.parse(raw) as { name?: string }
    if (pkg.name) return sanitize(pkg.name)
  } catch {
    // 读不到就用兜底名字，不值得为此打断启动
  }
  return 'web'
}

/**
 * 转成隧道名允许的形式：小写字母、数字、连字符。
 *
 * 这个规则要和服务端对上，否则用户会拿到一句莫名其妙的拒绝。
 */
export function sanitize(raw: string): string {
  const cleaned = raw
    .replace(/^@[^/]+\//, '') // 去掉 npm scope
    .toLowerCase()
    .replace(/[^a-z0-9-]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 40)
  return cleaned || 'web'
}

/** 把隧道域名并进用户已有的 allowedHosts 配置。 */
export function mergeAllowedHosts(
  existing: string[] | true | undefined,
  suffix: string,
): string[] | true {
  // 用户已经全放开了，别多此一举
  if (existing === true) return true
  const list = existing ?? []
  return list.includes(suffix) ? list : [...list, suffix]
}

async function get<T>(url: string): Promise<T | undefined> {
  return request<T>(url, { method: 'GET' })
}

async function post<T>(url: string, body: unknown): Promise<T | undefined> {
  return request<T>(url, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  })
}

async function request<T>(url: string, init: RequestInit): Promise<T | undefined> {
  try {
    const response = await fetch(url, {
      ...init,
      signal: AbortSignal.timeout(TIMEOUT_MS),
    })
    if (!response.ok) return undefined
    return (await response.json()) as T
  } catch {
    // 穿云没跑、超时、连接被拒——都归为「用不上隧道」，静默降级。
    // 让 dev server 因为一个可选功能起不来是不可接受的。
    return undefined
  }
}

/**
 * 同步注销。
 *
 * 进程退出时事件循环已经不转了，异步请求发不出去。这里借 curl 同步发一枪。
 *
 * 是**关掉**，不是 DELETE：DELETE 会把这条隧道连同用户在穿云客户端里给它设的
 * 访问口令一起删掉，下次 dev server 起来就是一条没门的隧道。关掉的隧道留在
 * 客户端列表里（开关是关的、口令还在），下次注册时原地打开。
 * 失败也不致命——同名隧道会在下次注册时被接管——但能让列表干净些。
 */
function unregisterSync(apiPort: number, name: string): void {
  try {
    const { execFileSync } = require('node:child_process') as typeof import('node:child_process')
    execFileSync(
      'curl',
      [
        '-s',
        '-m',
        '2',
        '-X',
        'PATCH',
        '-H',
        'Content-Type: application/json',
        '-d',
        '{"enabled":false}',
        `http://127.0.0.1:${apiPort}/api/tunnels/${encodeURIComponent(name)}`,
      ],
      { stdio: 'ignore' },
    )
  } catch {
    // 没有 curl、或者穿云已经关了——都不影响什么
  }
}

const ESC = '\x1b'
const dim = (s: string) => `${ESC}[2m${s}${ESC}[22m`
const bold = (s: string) => `${ESC}[1m${s}${ESC}[22m`
const cyan = (s: string) => `${ESC}[36m${s}${ESC}[39m`
const yellow = (s: string) => `${ESC}[33m${s}${ESC}[39m`
