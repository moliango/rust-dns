# rust-dns 0.1.1 发布说明

0.1.0 首次可用版本暴露的问题与 0.1.1 的修复清单。

> 0.1.2 的打包清理见 [`RELEASE-NOTES-0.1.2.md`](RELEASE-NOTES-0.1.2.md)。

## 一、构建 / CI / OpenWrt 打包

| # | 问题 | 修复 |
|---|---|---|
| 1 | PAT 缺 `workflow` scope，无法推送 workflow | 使用带 `workflow` scope 的凭据；与代码无关 |
| 2 | armv7 用 cargo-zigbuild 构建失败（ring/musl） | armv7 改用 `cross`，x86_64/aarch64 保留 zigbuild |
| 3 | 发布 tarball 顶层是单个 `rust-dns` 文件，OpenWrt `PKG_UNPACK` 解压后路径不对 | tarball 包一层 `rust-dns-<version>-<arch>/rust-dns` |
| 4 | OpenWrt feed 名 `rust-dns` 含连字符，`scripts/feeds` 语法错误 | feed 名改为 `rustdns`，包名仍为 `rust-dns` |
| 5 | `gh-action-sdk` 对不存在的 `ARTIFACTS_DIR` 做 chown，SDK 启动前失败 | 运行前创建目录；后改为直接运行 SDK 容器 |
| 6 | SDK 容器无法写挂载的产物目录 | `chmod -R a+rwX feed sdk-artifacts` |
| 7 | SDK 把 `bin/` 移到 `sdk-artifacts/`，`find bin` 在 `set -e` 下提前退出 | 只遍历存在的目录 |
| 8 | 收集了 SDK 默认 feed 的全部包，Release 被塞进 328 个依赖包 | 只收集 `rust-dns`、`luci-app-rust-dns`、`luci-i18n-rust-dns-*`、`geo2txt` |
| 9 | OpenWrt 32 位 ARM 内部 `ARCH=arm`，被误判为 armv5/armv6 | 由 CI 显式传 `RUST_DNS_ARCH=armv7`，Makefile 用 `?=` |
| 10 | 包没有校验和 / 升级无法验证下载文件 | 0.1.1 生成 `SHA256SUMS-packages` 并随 Release 发布 |
| 11 | 服务包只装二进制，更新工具依赖的 `geo2txt` 不在源里 | 0.1.1 自带 `geo2txt` 包并声明依赖 |

## 二、LuCI / rpcd / 运行时

| # | 问题 | 修复 |
|---|---|---|
| 12 | rpcd ucode 文件用了 `throw`，ucode 不支持，整个 `luci.rust_dns` 加载失败 → Update/Cloudflare/Logs API 全不可用 | 改为 `die()`；用真实 ucode 编译检查 |
| 13 | ucode 字符类里的 `\s` 被当成字面字母 `s`，默认测速 URL 被误判为含空白/引号，列表也被按 `s` 分割 | 用显式空白字符类 `[ \t\r\n\f\v]` |
| 14 | 安装 LuCI 包后 rpcd 没重启，插件对象不存在 | LuCI 包加 `postinst` 重启 rpcd/uhttpd，并显式依赖 `rpcd-mod-ucode` |
| 15 | init 只把 stdout/stderr 交给 syslog，`/var/log/rust-dns.log` 不存在，Logs 页报错 | 启动时创建日志文件并把 daemon 输出重定向进去 |
| 16 | 更新工具调用不存在的 `v2dat_dump` | 改为调用 `geo2txt geosite|geoip -f <dat> -e <tag> -o <dir>` |
| 17 | 用户源里没有 `geo2txt` | 0.1.1 自带 `geo2txt` 包并声明依赖 |
| 18 | 没有本地 `.dat` 且 `geo_auto_update=0` 时无法更新 | 手动更新在本地 dat 缺失时自动下载 dat |
| 19 | 默认 5335 / 9091 与 mosdns 冲突，服务起不来 → API 不可达 | init 同时检查 DNS 与 API 端口并提示 5336/9092；文档补充共存配置 |
| 20 | Cloudflare 页面因 status RPC 失败误报“cf-optimize 子命令不存在” | rpcd 修复后状态 API 正常；错误提示只反映真实 RPC 结果 |
| 21 | 没有从 GitHub 升级的入口 | 0.1.1 增加 LuCI “System Upgrade” 与 `rust-dns.uc upgrade` |

## 三、0.1.1 的下载与升级

- Release 会同时提供：服务包、LuCI 包、中文翻译包、`geo2txt` 包。
- OpenWrt 23.05/24.10 ipk、25.12 apk 的架构矩阵保持不变。
- 从 0.1.0 升级到 0.1.1 可以：

```sh
# 路由器上，用 Release 里的包
opkg install --force-reinstall ./geo2txt_*.ipk ./rust-dns_0.1.1-*.ipk ./luci-app-rust-dns_0.1.1-*.ipk
# 或升级到 0.1.1 后，用 LuCI 的 System Upgrade 页从 GitHub 检查/升级
```

> 升级功能会从固定的 GitHub 仓库读取 Release，只接受匹配当前 OpenWrt 分支/架构的包，并用 `SHA256SUMS-packages` 校验后安装。
