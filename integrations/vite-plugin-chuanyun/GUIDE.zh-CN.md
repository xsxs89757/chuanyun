# vite-plugin-chuanyun

[English](README.md) · [中文](GUIDE.zh-CN.md)

把 Vite 的 dev server 自动接进[穿云](https://github.com/xsxs89757/chuanyun)隧道。

## 安装

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
<summary>也可以不经 npm 安装</summary>

pnpm 支持从仓库子目录直接装：

```bash
pnpm add -D "github:xsxs89757/chuanyun#path:/integrations/vite-plugin-chuanyun"
```

npm 和 yarn 不支持子目录，得先打个包：

```bash
git clone https://github.com/xsxs89757/chuanyun.git
cd chuanyun/integrations/vite-plugin-chuanyun
npm install && npm pack        # 得到 vite-plugin-chuanyun-x.y.z.tgz

cd /你的前端项目
npm i -D /path/to/vite-plugin-chuanyun-x.y.z.tgz
```

</details>

## 用法

```ts
// vite.config.ts
import { defineConfig } from 'vite'
import chuanyun from 'vite-plugin-chuanyun'

export default defineConfig({
  plugins: [chuanyun({ name: 'admin' })],
})
```

启动 dev server 就会多出一行：

```
  ➜  Local:   http://localhost:5173/
  ➜  穿云:    https://zhangsan-admin.t.example.com
```

那个地址是公网可达的固定地址，可以直接填进微信后台、发给同事、用手机打开。

## 它替你做了什么

**放行隧道域名。** Vite 默认会拦下不认识的 Host，经隧道访问时你会撞上
`Blocked request: this host is not allowed`。插件把隧道域名加进 `allowedHosts`，
不用你手配（你自己配的那些会保留，不会被覆盖）。

**用实际端口，而不是配置里写的。** 5173 被占时 Vite 会自动挪到 5174，
写死端口的隧道就指错了地方。插件等 dev server 真正 listening 之后再注册。

**退出时注销。** 关掉 dev server，隧道也跟着下线。

## 选项

| 选项 | 默认 | 说明 |
|---|---|---|
| `name` | package.json 的项目名 | 隧道名，会变成地址的一部分。`@company/admin-panel` 会取成 `admin-panel` |
| `apiPort` | `7075` | 穿云客户端的本地接口端口 |
| `auth` | 无 | 访问口令（`用户名:口令`），给隧道加一道门 |
| `enabled` | `true` | 设成 `false` 可在 CI 等环境里跳过 |

按环境开关：

```ts
plugins: [chuanyun({ enabled: !process.env.CI })]
```

## 前提

需要本机装着并登录了[穿云客户端](https://github.com/xsxs89757/chuanyun)。

**没装或没登录也不影响**——插件打一行提示就让路，dev server 照常启动，
`localhost` 照常能用。一个可选功能不该让本地开发起不来，所以连不上、超时、
没登录这些情况全部静默降级。

## HMR

现代 Vite 的 HMR 按页面 origin 建 WebSocket，经隧道就是 `wss://`，穿云会透传，
无需额外配置。老版本可能需要：

```ts
server: { hmr: { clientPort: 443 } }
```

## 许可

Apache-2.0
