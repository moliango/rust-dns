# rust-dns

[![CI and Release](https://github.com/moliango/rust-dns/actions/workflows/release.yml/badge.svg)](https://github.com/moliango/rust-dns/actions/workflows/release.yml)
[![License: GPL-3.0](https://img.shields.io/badge/license-GPL--3.0-blue.svg)](LICENSE)

[English](README.md) | **简体中文**

用 Rust 重写的 [mosdns](https://github.com/IrineSistiana/mosdns) 风格 DNS 转发器，面向 OpenWrt：
插件化配置、LuCI 管理页面、本地统计 API，以及 Cloudflare 优选 IP 工具。

> **状态**：DNS 核心 MVP 与管理端均已实现并通过测试：UDP/TCP 监听、`sequence` 子集、
> UDP/TCP/DoT 上游、TTL 缓存、stats 打点、LuCI 六页、OpenWrt 服务包与 Cloudflare 优选。
> DoT/DoH/DoQ 服务端、DoH 上游、IP_PKTINFO、多 worker、ECS/双栈优选和完整 geoip 语义属于后续工作。

## 功能

- **DNS 转发核心**
  - UDP / TCP 服务端，`udp://`、`tcp://`、`tls://`（DoT）上游。
  - `sequence` 规则子集：域名/IP matcher、hosts、redirect、白/黑/灰名单、reject、cache、响应修饰。
  - TTL 缓存（命中扣减、lazy TTL、flush）；统计事件写入 `/api/v1`。
- **LuCI 管理页面（6 页）**
  - Basic、Rules、Geodata Update、Statistics、Logs、Cloudflare 优选。
  - MosDNS 配置一键导入、CodeMirror YAML 编辑器、规则单条增删。
- **本地统计/管理 API**
  - `/api/v1/{health,version,stats,history,top,logs,logs/clear,cache/flush}`，仅监听回环。
- **Cloudflare 优选**
  - 延迟 + 下载测速、官方 IP 段抓取缓存、`SO_MARK`/设备绑定、直连校验。
  - 结果列表点击/批量写入 rust-dns hosts 受管块，reload 生效。
- **OpenWrt 打包**
  - 服务包 `rust-dns` + 前端包 `luci-app-rust-dns`。
  - 目标：OpenWrt 23.05+（opkg）、24.10（opkg）、25.12/main（apk）。
  - 架构：x86_64、aarch64、armv7（v1 不含 MIPS/armv5）。

## 目录

```text
rust-dns/                     Rust workspace（Rust 1.100.0，MSRV 1.88）
├── crates/rust-dns/          CLI: version/check/start/stats-api/cf-optimize/cf-apply-hosts
├── crates/rust-dns-core/     DNS 核心：config/context/matcher/sequence/cache/upstream/server
├── crates/rust-dns-stats-api/    /api/v1 collector + HTTP API
└── crates/rust-dns-cf-optimizer/ Cloudflare 优选、绕过校验、hosts 合并
openwrt/rust-dns/             OpenWrt 服务包（init、UCI、config.sh、rust-dns.uc、规则）
luci-app-rust-dns/            LuCI 前端（六页、rpcd、ACL、po、CodeMirror）
dev/                          mock API 与本地 stub（不安装）
dist/                         本地 release 产物（不提交）
```

## 快速开始

### 1. 直接使用 release 二进制

从 [Releases](https://github.com/moliango/rust-dns/releases) 下载对应架构的
`rust-dns-<version>-<arch>.tar.gz`，解压后：

```sh
tar -xzf rust-dns-<version>-<arch>.tar.gz
cd rust-dns-<version>-<arch>
./rust-dns version
./rust-dns check -c /path/to/config.json
./rust-dns start -c /path/to/config.json -d /var/rust-dns

# 另一个终端
dig @127.0.0.1 -p 5335 example.com
```

生成的配置由 OpenWrt 服务包的 `config.sh` 产出（见下文）。

### 2. 从源码构建

```sh
rustup toolchain install 1.100.0
cd rust-dns
cargo test --workspace
cargo build --release
```

静态 musl（本机 aarch64 示例）：

```sh
sudo apt-get install musl-tools
CC_aarch64_unknown_linux_musl=musl-gcc \
  cargo build --release --target aarch64-unknown-linux-musl
file target/aarch64-unknown-linux-musl/release/rust-dns   # statically linked
```

跨架构推荐 `cargo-zigbuild`（GitHub Actions 已使用）：

```sh
cargo install cargo-zigbuild
cargo zigbuild --release --target aarch64-unknown-linux-musl -p rust-dns
cargo zigbuild --release --target armv7-unknown-linux-musleabihf -p rust-dns
```

### 3. OpenWrt 包

详细安装步骤（架构选择、opkg/apk、验证、排障）：
[`docs/INSTALL-openwrt.md`](docs/INSTALL-openwrt.md)。

GitHub Release 会附带 CI 构建好的 OpenWrt 包：

- **23.05.5 / 24.10.8 ipk**：x86_64、aarch64_generic、aarch64_cortex-a53、
  arm_cortex-a7_neon-vfpv4。
- **25.12.6 apk**：x86_64。

下载与你的 OpenWrt 版本、包架构匹配的文件。Release 文件名带 OpenWrt 版本和
目标架构前缀以避免重名，例如
`24.10.8_x86_64-24.10.8_rust-dns_0.1.0-r1_x86_64.ipk`。直接安装即可：

```sh
opkg install *rust-dns_*.ipk *luci-app-rust-dns_*.ipk
# apk 版本：
apk add --allow-untrusted *rust-dns-*.apk *luci-app-rust-dns-*.apk
```

`luci-i18n-rust-dns-*` 是可选的中文界面翻译包。

如需自行构建，把 `openwrt/rust-dns` 和 `luci-app-rust-dns` 放进 OpenWrt SDK 的
`package/`，把 release tarball 的 SHA256 写入
`openwrt/rust-dns/hashes/<arch>.sha256`，然后：

```sh
make package/rust-dns/compile V=s
make package/luci-app-rust-dns/compile V=s
```

也可以直接使用 release 里的 `rust-dns-<version>-openwrt-src.tar.gz` 作为本地 feed 源。

## 配置模型

OpenWrt 上 UCI 是入口，`/etc/init.d/rust-dns` 调用 `config.sh` 生成
`/var/etc/rust-dns/config.json`，再由 `rust-dns check` 校验：

```text
/etc/config/rust-dns
        │ config.sh
        ▼
/var/etc/rust-dns/config.json  ── rust-dns check ──▶ procd start
        │
        ├─ udp_server / tcp_server → DNS listeners
        ├─ sequence → matcher / hosts / redirect / forward / cache
        ├─ forward → udp:// / tcp:// / tls:// upstreams
        └─ stats_collector → /api/v1
```

关键 UCI 字段、默认值和生成 JSON 结构见
`openwrt/rust-dns/files/rust-dns.config` 与 `openwrt/rust-dns/files/config.sh`。

`sequence` 规则是字符串数组，例如：

```json
"rules": [
  "hosts",
  "whitelist -> forward_local",
  "blocklist -> reject",
  "local-ptr -> reject",
  "ddnslist -> forward_local",
  "streaming -> forward_stream",
  "geosite_cn -> forward_local",
  "geoip_cn -> forward_local",
  "greylist -> forward_remote",
  "redirect",
  "modify_response",
  "cache",
  "forward_remote"
]
```

## LuCI 页面

| 页面 | 内容 |
|---|---|
| Basic | 监听、日志、上传、缓存、ECS/TTL、API、自定义 YAML |
| Rules | 8 类规则文件 + 单条增删 |
| Geodata Update | geosite/geoip/adlist 更新、cron、代理 |
| Statistics | QPS、命中率、Top 域名/客户端、查询日志 |
| Logs | 日志轮询、级别/关键字过滤、清空 |
| Cloudflare 优选 | 测速、绕过 OpenClash、结果表、写入 hosts |

LuCI 包只包含 UI/rpcd；`/etc/config/rust-dns`、规则、init、`config.sh` 由
`rust-dns` 服务包安装。

## Stats / Admin API

默认 `127.0.0.1:9091`：

```sh
curl -s http://127.0.0.1:9091/api/v1/health
curl -s http://127.0.0.1:9091/api/v1/stats
curl -s 'http://127.0.0.1:9091/api/v1/logs?limit=20&offset=0&filter=all'
curl -s -X POST http://127.0.0.1:9091/api/v1/cache/flush
```

v1 只允许回环绑定；非回环地址会被 `config.sh`/CLI 拒绝。

## Cloudflare 优选

CLI：

```sh
rust-dns cf-optimize --source cloudflare --family 4 \
  --output /var/rust-dns/cf-optimizer/last.json \
  --progress /var/rust-dns/cf-optimizer/progress.json \
  --cancel-file /var/rust-dns/cf-optimizer/cancel
```

写入 hosts（受管块，自动 `.bak`，可选 reload）：

```sh
rust-dns cf-apply-hosts \
  --file /etc/rust-dns/rule/hosts.txt \
  --reload-init /etc/init.d/rust-dns \
  --entry cloudflare.com=104.16.0.1
```

LuCI 的「Cloudflare 优选」页把上述能力图形化：参数、进度、排序/过滤、勾选、
行内或批量写入 hosts。OpenClash 绕行是**尽力而为**：
`--bypass-mode mark` 使用 `SO_MARK` + 临时 `ip rule`/直连路由，`device` 绑定
WAN 接口；直连校验失败时 UI 会警告，必要时可显式暂停 OpenClash。所有临时规则
在结束/取消/异常时清理。

## GitHub Actions

`.github/workflows/release.yml`：

- push/PR：`cargo fmt`、`clippy -D warnings`、`cargo test --workspace`、LuCI JS 语法、
  mock API/shell 语法。
- tag `v*` 或手动触发：三架构静态 musl 构建，生成
  `rust-dns-<version>-<arch>.tar.gz` + `.sha256`，上传 GitHub Release；
  同时生成 `rust-dns-<version>-openwrt-src.tar.gz`。
- 同一 workflow 用 `openwrt/gh-action-sdk` 构建 OpenWrt ipk/apk 并附到 Release：
  23.05.5/24.10.8 的 x86_64、aarch64_generic、aarch64_cortex-a53、
  arm_cortex-a7_neon-vfpv4 ipk，以及 25.12.6 x86_64 apk。
- 发布：

```sh
git tag v0.1.0
git push origin v0.1.0
```

发布后把 release 的 SHA256 填回 `openwrt/rust-dns/hashes/<arch>.sha256`，
再构建 ipk/apk（或让打包流程读取 release 资产）。

## 开发与测试

```sh
cd rust-dns
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

LuCI / 服务包：

```sh
node --check luci-app-rust-dns/htdocs/luci-static/resources/view/rust-dns/*.js
python3 -m py_compile dev/mock-api/server.py
sh openwrt/rust-dns/tests/config_generator/run.sh
```

DNS 核心测试全部使用进程内假上游/临时端口，不依赖外网或系统 `dig`。
OpenWrt SDK、`ucode`、`shellcheck`、`msgfmt` 未包含在本机环境，相关检查只能在
CI/实机执行。

## 已知限制 / Roadmap

- DNS 核心 MVP 之外的插件（DoH/DoQ 服务端、DoH 上游、IP_PKTINFO、多 worker、
  ECS/双栈优选、完整 geoip 语义）尚未实现。
- OpenWrt 23.05/24.10/25.12 的 ipk/apk 实机安装、LuCI 浏览器渲染、ucode 运行时
  校验需在 CI/路由器上补做。
- OpenClash 的 mark/device/pause 三种模式需要在真实 OpenClash 环境验证。
- MIPS/armv5 不在 v1 目标内（Rust 无预编译 musl std）。

## 许可证

GPL-3.0-only。LuCI 前端与 OpenWrt 打包部分改编自
[luci-app-mosdns](https://github.com/sbwml/luci-app-mosdns)（GPL-3.0），
版权与来源声明保留在对应文件中。CodeMirror/js-yaml 的第三方许可见
`luci-app-rust-dns/root/usr/share/licenses/luci-app-rust-dns/THIRD-PARTY-NOTICES`。
