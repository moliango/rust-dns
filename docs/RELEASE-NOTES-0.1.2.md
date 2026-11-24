# rust-dns 0.1.2 发布说明

0.1.1 发布后，我下载并逐个检查了 CI 产出的真实 ipk/apk，发现 LuCI 包有两个
打包内容问题。它们不影响 DNS 核心，但会让安装包不够干净、并在极简系统上缺少
显式依赖，因此发布 0.1.2。

> 注意：本版 LuCI 包的文件权限是 `0777`，rpcd 会忽略这样的 ucode 脚本，
> 导致 LuCI 页面报“API 不可达”。修复与临时处理方法见
> [`RELEASE-NOTES-0.1.3.md`](RELEASE-NOTES-0.1.3.md)。

0.1.1 的功能与 21 项修复清单见 [`RELEASE-NOTES-0.1.1.md`](RELEASE-NOTES-0.1.1.md)。

## 一、本版修复

| # | 问题 | 影响 | 修复 |
|---|---|---|---|
| 1 | LuCI 的 jsmin 步骤无法压缩已经压缩过的 `js-yaml.min.js`，会把半成品留在包里：`js-yaml.min.js.o` | LuCI 包多一个约 39 KB 的无用文件，且原 `.js` 并没有被真正压缩 | 本包设置 `LUCI_MINIFY_JS:=0`，只打包 `htdocs` 里的原始文件 |
| 2 | `luci-app-rust-dns` 没有显式依赖 `luci-base` | 在极简系统上可能先装上 UI 包却缺少 `baseclass`/`form`/`rpc` 运行库 | `LUCI_DEPENDS` 增加 `+luci-base` |

### 0.1.1 已验证正常、本版保持不变的部分

- `postinst` / `prerm` 的 Makefile 变量由 OpenWrt 构建系统写成
  `CONTROL/postinst-pkg` / `CONTROL/prerm-pkg`，安装后位于
  `/usr/lib/opkg/info/<pkg>.postinst-pkg`，由 `default_postinst()` /
  `default_prerm()` source 执行；LuCI 包安装/升级后确实会重启 rpcd、uhttpd。
- `rust-dns` 依赖 `geo2txt`，Release 自带对应架构的 `geo2txt` 包。
- `SHA256SUMS-packages` 覆盖全部 32 个 ipk/apk，升级页会逐个校验。
- conffiles 覆盖 `/etc/config/rust-dns`、`/etc/rust-dns/config_custom.yaml`
  和规则文件，升级保留用户配置。

## 二、安装 / 升级到 0.1.2

### 路由器上用包文件安装

先装 `geo2txt`，再装服务包，最后装 LuCI：

```sh
cd /tmp
BASE=https://github.com/moliango/rust-dns/releases/download/v0.1.2

# 以 24.10.8 x86_64 为例，其他版本/架构替换前缀
wget "$BASE/24.10.8_x86_64-24.10.8_geo2txt_1.0.0-r1_x86_64.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_rust-dns_0.1.2-r1_x86_64.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.2-r1_all.ipk"

opkg install ./24.10.8_x86_64-24.10.8_geo2txt_1.0.0-r1_x86_64.ipk
opkg install ./24.10.8_x86_64-24.10.8_rust-dns_0.1.2-r1_x86_64.ipk
opkg install ./24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.2-r1_all.ipk
```

25.12 / apk：

```sh
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_geo2txt-1.0.0-r1.apk
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_rust-dns-0.1.2-r1.apk
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_luci-app-rust-dns-0.1.2-r1.apk
```

### 从 0.1.1 用 LuCI 升级

**服务 → Rust DNS → System Upgrade** → 检查更新 → 升级。该页会：

1. 从固定的 `moliango/rust-dns` GitHub 仓库读取最新 Release；
2. 只挑选当前 OpenWrt 分支（23.05 / 24.10 / 25.12）和 `DISTRIB_ARCH` 匹配的包；
3. 先装 `geo2txt`，再装 `rust-dns` 和 `luci-app-rust-dns`；
4. 用 Release 的 `SHA256SUMS-packages` 校验每个下载文件；
5. 升级后重启已启用的 rust-dns 服务以及 rpcd/uhttpd。

如果 0.1.1 安装后 LuCI 页面数据一直为空，先执行一次
`/etc/init.d/rpcd restart` 再进升级页；升级到 0.1.2 后安装脚本会自动处理。

## 三、验证过的内容

- Release 共 32 个 ipk/apk 包，`SHA256SUMS-packages` 与全部包文件对应。
- 抽查 24.10.8 x86_64 与 25.12.6 x86_64 的 `geo2txt`、`rust-dns`、
  `luci-app-rust-dns`：SHA256 全部匹配。
- ipk 控制信息：`rust-dns` 依赖 `geo2txt`、`ucode`、`ucode-mod-fs`、
  `ucode-mod-uci`、`uclient-fetch`、`ca-bundle`；`luci-app-rust-dns` 依赖
  `luci-base`、`rust-dns`、`rpcd-mod-ucode`、`uclient-fetch`。
- control 脚本：包内包含 `postinst-pkg` / `prerm-pkg`，由默认脚本 source。
- LuCI 包包含七个页面（含 `upgrade.js`），0.1.2 不再包含 `*.js.o`。

> 仍未在真实路由器上执行安装/升级；CI 负责构建与打包，实机验证需要你在路由器上完成。
