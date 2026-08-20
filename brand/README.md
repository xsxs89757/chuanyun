# 品牌注入

穿云是开源项目，但一个部署方（比如某家公司）希望同事**装上就能用**：不填服务器地址、不核对证书指纹。这两件事互相冲突——服务器地址和证书指纹属于部署方的私有信息，不该出现在公开仓库里。

品牌注入就是解法：仓库里只有通用的 [`default.toml`](default.toml)，部署方在自己的 CI 里用一份私有配置替换它再编译。

## 怎么用

1. 复制 `default.toml` 为 `company.toml`（这个文件名已被 `.gitignore` 排除，不会误提交）；
2. 填入真实的 `default_server` 与 `tls_pin`（服务端首次启动时会打印指纹）；
3. 构建时指定：

```bash
CHUANYUN_BRAND=brand/company.toml cargo build --release -p cy-desktop
```

不指定 `CHUANYUN_BRAND` 时用 `default.toml`，产出的就是开源通用版：用户自己填服务器地址，首次连接时确认证书指纹（TOFU）。

## 运行时优先级

用户在界面里的设置 > 编译内嵌的品牌 > 缺省值。

也就是说，内部版的同事仍然可以手动改服务器地址（比如临时连测试环境），品牌只是提供一个"开箱即用"的起点，不是硬编码的锁。
