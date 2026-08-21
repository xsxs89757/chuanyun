import { strict as assert } from 'node:assert'
import { test } from 'node:test'
import http from 'node:http'

import chuanyun, { sanitize, mergeAllowedHosts } from '../dist/index.js'

test('隧道名会被转成服务端接受的形式', () => {
  // npm 包名五花八门，隧道名只认小写字母数字连字符——
  // 规则对不上的话，用户会收到一句莫名其妙的拒绝
  assert.equal(sanitize('@company/admin-panel'), 'admin-panel')
  assert.equal(sanitize('My_App'), 'my-app')
  assert.equal(sanitize('web.client'), 'web-client')
  assert.equal(sanitize('---weird---'), 'weird')
  assert.equal(sanitize('中文项目'), 'web', '全是不合法字符时退回兜底名')
  assert.equal(sanitize('a'.repeat(80)).length, 40, '过长要截断')
})

test('allowedHosts 合并不会覆盖用户已有的配置', () => {
  assert.deepEqual(mergeAllowedHosts(undefined, '.t.example.com'), ['.t.example.com'])
  assert.deepEqual(
    mergeAllowedHosts(['localhost'], '.t.example.com'),
    ['localhost', '.t.example.com'],
    '用户原来配的要留着',
  )
  assert.deepEqual(
    mergeAllowedHosts(['.t.example.com'], '.t.example.com'),
    ['.t.example.com'],
    '不该重复添加',
  )
  assert.equal(
    mergeAllowedHosts(true, '.t.example.com'),
    true,
    '用户已经全放开了就别多事',
  )
})

test('插件只在 dev 时生效', () => {
  const plugin = chuanyun()
  assert.equal(plugin.name, 'vite-plugin-chuanyun')
  assert.equal(plugin.apply, 'serve', 'build 产物跟隧道没关系')
})

test('enabled: false 时什么都不做', async () => {
  const plugin = chuanyun({ enabled: false })
  const result = await plugin.config({}, { command: 'serve' })
  assert.equal(result, undefined, 'CI 里要能干净地跳过')
})

test('穿云没运行时静默降级，不拖累 dev server', async () => {
  // 指向一个没人监听的端口
  const plugin = chuanyun({ apiPort: 1 })
  const result = await plugin.config({}, { command: 'serve' })
  assert.equal(
    result,
    undefined,
    '连不上穿云不该抛错——dev server 不能因为一个可选功能起不来',
  )
})

test('拿到域名后缀后会自动放行隧道域名', async () => {
  const server = http.createServer((req, res) => {
    if (req.url === '/api/status') {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(
        JSON.stringify({
          connected: true,
          domain_suffix: 't.example.com',
          needs_login: false,
        }),
      )
      return
    }
    res.writeHead(404).end()
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const port = server.address().port

  try {
    const plugin = chuanyun({ apiPort: port })
    const result = await plugin.config({}, { command: 'serve' })
    assert.deepEqual(
      result.server.allowedHosts,
      ['.t.example.com'],
      'Vite 默认会拦下隧道域名，这一步就是为了免掉那句 Blocked request',
    )
  } finally {
    server.close()
  }
})

test('已有 allowedHosts 时并进去而不是替换', async () => {
  const server = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(
      JSON.stringify({
        connected: true,
        domain_suffix: 't.example.com',
        needs_login: false,
      }),
    )
  })
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve))
  const port = server.address().port

  try {
    const plugin = chuanyun({ apiPort: port })
    const result = await plugin.config(
      { server: { allowedHosts: ['my.local'] } },
      { command: 'serve' },
    )
    assert.deepEqual(result.server.allowedHosts, ['my.local', '.t.example.com'])
  } finally {
    server.close()
  }
})

test('注册用的是 dev server 的实际端口，而不是配置里写的', async () => {
  let registered
  const api = http.createServer((req, res) => {
    if (req.url === '/api/status') {
      res.writeHead(200, { 'content-type': 'application/json' })
      res.end(
        JSON.stringify({
          connected: true,
          domain_suffix: 't.example.com',
          needs_login: false,
        }),
      )
      return
    }
    if (req.url === '/api/tunnels' && req.method === 'POST') {
      let body = ''
      req.on('data', (c) => (body += c))
      req.on('end', () => {
        registered = JSON.parse(body)
        res.writeHead(200, { 'content-type': 'application/json' })
        res.end(
          JSON.stringify([
            {
              name: registered.name,
              ok: true,
              url: `https://zhangsan-${registered.name}.t.example.com`,
            },
          ]),
        )
      })
      return
    }
    res.writeHead(404).end()
  })
  await new Promise((resolve) => api.listen(0, '127.0.0.1', resolve))
  const apiPort = api.address().port

  // 假装一个已经在监听的 dev server——端口由内核给，模拟"5173 被占挪到别处"
  const devServer = http.createServer()
  await new Promise((resolve) => devServer.listen(0, '127.0.0.1', resolve))
  const actualPort = devServer.address().port

  const logs = []
  const fakeViteServer = {
    httpServer: devServer,
    config: {
      logger: {
        info: (m) => logs.push(m),
        warn: (m) => logs.push(m),
      },
    },
  }

  try {
    const plugin = chuanyun({ apiPort, name: 'admin' })
    plugin.configureServer(fakeViteServer)
    devServer.emit('listening')

    // 等注册请求打过来
    for (let i = 0; i < 50 && !registered; i++) {
      await new Promise((r) => setTimeout(r, 20))
    }

    assert.ok(registered, '应当发出注册请求')
    assert.equal(
      registered.port,
      actualPort,
      '端口会漂（5173 被占就换一个），必须用实际监听的那个',
    )
    assert.equal(registered.name, 'admin')
    assert.ok(
      logs.some((l) => l.includes('zhangsan-admin.t.example.com')),
      `公网地址要打出来给用户看，实际日志：${JSON.stringify(logs)}`,
    )
  } finally {
    devServer.close()
    api.close()
  }
})

test('还没登录时给出提示但不中断启动', async () => {
  const api = http.createServer((req, res) => {
    res.writeHead(200, { 'content-type': 'application/json' })
    res.end(
      JSON.stringify({ connected: false, domain_suffix: '', needs_login: true }),
    )
  })
  await new Promise((resolve) => api.listen(0, '127.0.0.1', resolve))
  const apiPort = api.address().port

  const devServer = http.createServer()
  await new Promise((resolve) => devServer.listen(0, '127.0.0.1', resolve))

  const logs = []
  const fakeViteServer = {
    httpServer: devServer,
    config: { logger: { info: (m) => logs.push(m), warn: (m) => logs.push(m) } },
  }

  try {
    const plugin = chuanyun({ apiPort, name: 'admin' })
    plugin.configureServer(fakeViteServer)
    devServer.emit('listening')
    await new Promise((r) => setTimeout(r, 300))

    assert.ok(
      logs.some((l) => l.includes('还没登录')),
      `应当提示用户，实际：${JSON.stringify(logs)}`,
    )
  } finally {
    devServer.close()
    api.close()
  }
})
