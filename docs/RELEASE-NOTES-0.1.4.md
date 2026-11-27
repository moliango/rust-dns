# rust-dns 0.1.4 发布说明

修复 0.1.1–0.1.3 中 Cloudflare 优选无法启动的问题：页面提示
“Unable to launch the optimizer process.”，但 rust-dns 服务本身正常。

## 一、根因

LuCI 后端启动优选进程时，把参数以 **数组** 形式传给 `fs.popen()`：

```ucode
let launch = [ '/bin/sh', '-c', wrapper, 'sh' ];
let p = popen(launch, 'r');        // 当前 OpenWrt 的 ucode 返回 null
```

而 OpenWrt 23.05 / 24.10 / 25.12 自带的 ucode 里，`fs.popen()` 只接受
**命令字符串**，传入数组会直接返回 `null`（errno `EINVAL`）。argv 数组
（execvp，无 shell）是 ucode 上游 2026-07-19 的提交
`fs: reimplement popen() with fork/exec and true shell-free array support`
才加入的，晚于目前所有 OpenWrt 发行版。

结果：

- `cf_start` 拿到 `null`，返回 “Unable to launch the optimizer process.”；
- 写 hosts 的 `cf-apply-hosts` 走同一个 helper，也一起失效；
- 其余走字符串 `popen()` 的功能（服务启停、数据库更新、升级检查）不受影响。

也就是说，这是 LuCI 前端与旧 ucode 的兼容性问题，与优选算法、
架构、网络都无关。

## 二、0.1.4 的修复

| # | 修复 |
|---|---|
| 1 | 新增 `shquote()` / `argv_to_command()`，把校验后的 argv 逐项单引号转义，再用字符串 `popen()` 执行；值里有空格、引号、`$()` 也不会被 shell 解释 |
| 2 | `cf_start` 改为 `命令 > 日志 2>&1 & echo $! > pid`，后台启动、记录 pid、立即返回 |
| 3 | `cf-apply-hosts` 的同步调用同样改为字符串命令，退出码仍原样返回 |
| 4 | CI 增加静态检查：源码里出现 `popen([ ... ])` 直接构建失败 |

## 三、升级到 0.1.4

优先手动安装（Release 附件 CDN 在部分网络不可达时升级页会失败）：

```sh
cd /tmp
BASE=https://github.com/moliango/rust-dns/releases/download/v0.1.4

# 24.10.8 x86_64 为例，其他版本/架构替换前缀；geo2txt 已装过不用重装
wget "$BASE/24.10.8_x86_64-24.10.8_rust-dns_0.1.4-r1_x86_64.ipk"
wget "$BASE/24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.4-r1_all.ipk"
opkg install ./24.10.8_x86_64-24.10.8_rust-dns_0.1.4-r1_x86_64.ipk
opkg install ./24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.4-r1_all.ipk
```

25.12 / apk 用 `apk add --allow-untrusted` 安装对应的 `-0.1.4-r1.apk`。

也可以走 LuCI **System Upgrade**；若路由器访问不了 GitHub 附件 CDN，
先在 **数据库更新** 页把 **GitHub Proxy** 填为 `https://gh-proxy.com/`
（结尾 `/` 必需）。

## 四、验证

- Cloudflare 优选页点击开始后不再报 “Unable to launch...”；
- `cat /var/rust-dns/cf-optimizer/run.pid` 能看到进程号，
  `run.log` 持续写入，页面进度正常推进；
- 取消按钮写入 `cancel` 文件后进程退出；
- 应用优选结果后 `/etc/rust-dns/rule/hosts.txt` 出现
  `# BEGIN/END rust-dns cf-optimizer` 管理块；
- 其余功能与 0.1.3 相同（0.1.3 修复了 LuCI 包 ucode 权限 `0777`
  导致 rpcd 不加载 `luci.rust_dns` 的问题）。
