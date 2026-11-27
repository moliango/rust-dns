# 安装 rust-dns 到 OpenWrt

Release: <https://github.com/moliango/rust-dns/releases/tag/v0.1.6>

每个平台有四个包：

- `geo2txt`：geosite/geoip `.dat` 解包工具，供 Geodata Update 使用
- `rust-dns`：服务包（二进制、init、UCI、规则、更新工具）
- `luci-app-rust-dns`：LuCI 七页管理界面
- `luci-i18n-rust-dns-zh-cn`：可选中文翻译

## 1. 确认 OpenWrt 版本和架构

```sh
cat /etc/openwrt_release
# DISTRIB_RELEASE='24.10.8'
# DISTRIB_ARCH='aarch64_cortex-a53'

# opkg 系统（23.05 / 24.10）
opkg print-architecture

# apk 系统（25.12）
apk --print-arch
```

对照 Release 里的前缀选择文件：

| OpenWrt | 包管理 | 选 `aarch64_generic` | 选 `aarch64_cortex-a53` | 选 `arm_cortex-a7...` | 选 `x86_64` |
|---|---|---|---|---|---|
| 23.05.5 | opkg | ✅ | — | ✅ | ✅ |
| 24.10.8 | opkg | ✅ | ✅ | ✅ | ✅ |
| 25.12.6 | apk | — | — | — | ✅ |

> 24.10 的 aarch64 路由器通常是 `aarch64_cortex-a53`（MT798x/IPQ807x 等），
> `aarch64_generic` 用于通用 ARMv8 镜像。选错会报 architecture 不兼容。

## 2. 下载

在 Release 页面下载对应版本/架构的三个文件，或用 SSH 在路由器上下载。
文件名带 `版本_架构-版本_` 前缀，例如 24.10.8 x86_64：

```text
24.10.8_x86_64-24.10.8_geo2txt_1.0.0-r1_x86_64.ipk
24.10.8_x86_64-24.10.8_rust-dns_0.1.6-r1_x86_64.ipk
24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.6-r1_all.ipk
24.10.8_x86_64-24.10.8_luci-i18n-rust-dns-zh-cn_0.261123.47855_all.ipk
```

路由器上下载（把文件名换成你的版本/架构）：

```sh
cd /tmp
BASE=https://github.com/moliango/rust-dns/releases/download/v0.1.6
wget "$BASE/24.10.8_x86_64-24.10.8_geo2txt_1.0.0-r1_x86_64.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_rust-dns_0.1.6-r1_x86_64.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.6-r1_all.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_luci-i18n-rust-dns-zh-cn_0.261123.47855_all.ipk"
```

国内网络访问 GitHub 慢时，可以在电脑上下载后用 `scp` 传到 `/tmp`，或者使用你信任的
GitHub 代理。不要从不明来源下载重新打包的 ipk。

## 3. 安装

先更新软件源，然后按 **geo2txt → 服务包 → LuCI** 的顺序安装：

```sh
# 23.05 / 24.10
opkg update
opkg install ./24.10.8_x86_64-24.10.8_geo2txt_1.0.0-r1_x86_64.ipk
opkg install ./24.10.8_x86_64-24.10.8_rust-dns_0.1.6-r1_x86_64.ipk
opkg install ./24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.6-r1_all.ipk
opkg install ./24.10.8_x86_64-24.10.8_luci-i18n-rust-dns-zh-cn_0.261123.47855_all.ipk  # 可选
```

```sh
# 25.12 / apk
apk update
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_geo2txt-1.0.0-r1.apk
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_rust-dns-0.1.6-r1.apk
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_luci-app-rust-dns-0.1.6-r1.apk
apk add --allow-untrusted ./25.12.6_x86_64-25.12.6_luci-i18n-rust-dns-zh-cn-*.apk  # 可选
```

`opkg`/`apk` 不会从本地文件解析依赖，`rust-dns` 依赖的 `geo2txt` 必须先单独安装。

如果提示缺少依赖：

```sh
opkg install ucode ucode-mod-fs ucode-mod-uci rpcd-mod-ucode uclient-fetch ca-bundle luci-base
# 或 apk add ucode ucode-mod-fs ucode-mod-uci rpcd-mod-ucode uclient-fetch ca-bundle luci-base
```

## 4. 启用并配置

服务包默认 `enabled=0`，需要启用：

### LuCI

重新登录 LuCI → **Services → Rust DNS → Basic Settings**：

1. 勾选 **Enabled**
2. 设置监听地址/端口（默认 `0.0.0.0:5335`）
3. 设置本地/远程 DNS 上游
4. 需要让 dnsmasq 使用 rust-dns 时，勾选 **DNS Forward**
5. **Save & Apply**

### 命令行

```sh
uci set rust-dns.config.enabled='1'
uci commit rust-dns
/etc/init.d/rust-dns enable
/etc/init.d/rust-dns start
```

让 dnsmasq 把查询转发给 rust-dns：

```sh
uci set rust-dns.config.redirect='1'
uci commit rust-dns
/etc/init.d/rust-dns restart
```

## 5. 验证

```sh
service rust-dns status
logread -e rust-dns | tail -20

# 监听端口（默认 5335）
netstat -lnup | grep 5335 || ss -lnup | grep 5335

# 直接查询
dig @127.0.0.1 -p 5335 example.com
# 没有 dig：opkg install bind-dig，或用 nslookup

# 查看生成配置
sed -n '1,40p' /var/etc/rust-dns/config.json
```

LuCI 菜单不出现时：

```sh
rm -rf /tmp/luci-*
# 退出并重新登录 LuCI，或重启
```

## 6. 升级 / 卸载

升级：直接安装新版本 ipk/apk，UCI、规则和 `config_custom.yaml` 属于 conffiles，会保留。
如果服务此前已启用，安装 `rust-dns` 时会自动重启守护进程。

也可以从 LuCI 的 **服务 → Rust DNS → System Upgrade** 检查固定 GitHub 仓库
（`moliango/rust-dns`）的新版本：它只选择与当前 OpenWrt 分支、目标架构匹配的
包，并用 Release 的 `SHA256SUMS-packages` 校验后再安装。

卸载：

```sh
opkg remove luci-app-rust-dns rust-dns geo2txt
# 或 apk del luci-app-rust-dns rust-dns geo2txt
```

卸载不会自动删除 `/etc/config/rust-dns`、`/etc/rust-dns/` 下的用户配置和规则。

## 7. 常见问题

| 现象 | 处理 |
|---|---|
| `package architecture ... not compatible` | 架构选错；按 `opkg print-architecture` / `apk --print-arch` 选 |
| 依赖 `ucode-mod-fs/uci` 缺失 | `opkg update` 后安装 `ucode ucode-mod-fs ucode-mod-uci luci-base` |
| 安装 `rust-dns` 提示缺少 `geo2txt` | 先安装 Release 里的本地 `geo2txt` 包，再装服务包 |
| 安装/升级 LuCI 后 Update/Cloudflare/Logs 不可用 | LuCI 包的 `postinst-pkg` 会重启 rpcd；仍异常时执行 `/etc/init.d/rpcd restart` |
| `ubus call luci.rust_dns get_stats` 报 `Command failed: Not found`，页面提示 API 不可达 | 0.1.2 的 LuCI 包文件权限为 `0777`，rpcd 会忽略；执行 `chmod 0644 /usr/share/rpcd/ucode/luci.rust_dns && /etc/init.d/rpcd restart`，或升级到 0.1.3 |
| Cloudflare 优选提示 `Unable to launch the optimizer process.` | 0.1.3 及更早版本把 argv 数组传给 `fs.popen()`，当前 OpenWrt 的 ucode 会返回 `null`；升级到 0.1.4 |
| 优选结果全是 `Failed`，或大部分是 `Not tested` | 0.1.4 及更早版本只从第一个 Cloudflare 网段取候选，0.1.5 改为全量网段采样。`Not tested` 表示未进入下载测速，不是失败；可在优选页调大 Download tests，透明代理下把 Bypass mode 设为 `mark` |
| 关闭 LuCI 标签页偶尔卡顿 | 0.1.5 及更早版本会一次渲染全部优选结果，并在标签页隐藏后继续轮询；升级到 0.1.6 |
| Geodata Update 报 `geo2txt: not found` | 未安装 `geo2txt`，按上面的顺序补装 |
| LuCI 没有 Rust DNS 菜单 | 清 `/tmp/luci-*`、重新登录；确认 `luci-app-rust-dns` 已安装 |
| 服务起不来 | 看 `/var/etc/rust-dns/last-error.log` 和 `logread -e rust-dns` |
| 5335 被占用 | Basic 改监听端口；或先停掉占用者 |
| 查询超时 | 检查上游 DNS 是否可达；远程 `tls://8.8.8.8` 在部分网络不可用，换本地/可用上游 |
| apk 提示未签名 | 使用 `apk add --allow-untrusted`（当前 Release 未签名） |
