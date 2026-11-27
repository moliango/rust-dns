# rust-dns 0.1.3 发布说明

> 0.1.3 的 Cloudflare 优选启动问题（`fs.popen()` 不接受 argv 数组）已在
> 0.1.4 修复，见 [`RELEASE-NOTES-0.1.4.md`](RELEASE-NOTES-0.1.4.md)。

修复 0.1.2 的一个 LuCI 打包缺陷：包内文件的权限被构建流程改成了 `0777`，
导致 rpcd 拒绝加载 `luci.rust_dns`，LuCI 的统计、升级、设置等页面都无法工作。

## 一、根因

CI 在运行 OpenWrt SDK 容器前执行了：

```sh
chmod -R a+rwX feed sdk-artifacts
```

`luci.mk` 安装 `root/` 和 `htdocs/` 时使用 `cp -pR`，会保留源文件权限。于是
`/usr/share/rpcd/ucode/luci.rust_dns` 以 `0777` 进入 ipk/apk。rpcd 的 ucode
插件在启动时会跳过所有人可写的脚本：

```c
if (s.st_mode & S_IWOTH) {
    fprintf(stderr, "Ignoring ucode script %s because it is world writable\n", path);
    continue;
}
```

结果是：

- rust-dns 进程、DNS、`/api/v1/health` 全部正常；
- `ubus call luci.rust_dns ...` 返回 `Command failed: Not found`；
- LuCI 页面提示“rust-dns API 不可达”或“升级检查 API 不可用”。

## 二、0.1.3 的修复

| # | 修复 |
|---|---|
| 1 | CI 只对产物目录 `sdk-artifacts` 放宽权限，不再 `chmod` 输入 feed，包内文件恢复为正常 `0644`/`0755` |
| 2 | LuCI 包的 `postinst-pkg` 在重启 rpcd 前把 `luci.rust_dns`、menu、ACL 显式 `chmod 0644`，即使安装器保留了异常权限也能自愈 |
| 3 | CI 新增检查：解包 `luci-app-rust-dns` ipk，确认 `luci.rust_dns` 没有 world-writable 位，否则构建失败 |

## 三、0.1.2 用户的临时修复

不用等 0.1.3，也不用重装：

```sh
chmod 0644 /usr/share/rpcd/ucode/luci.rust_dns
/etc/init.d/rpcd restart
ubus -v list luci.rust_dns | head
```

对象出现后退出并重新登录 LuCI 即可。确认 API 本身正常：

```sh
uclient-fetch -q -O - http://127.0.0.1:9091/api/v1/health
```

## 四、升级到 0.1.3

```sh
/usr/share/rust-dns/rust-dns.uc upgrade --check
/usr/share/rust-dns/rust-dns.uc upgrade
```

或在 LuCI：**服务 → Rust DNS → System Upgrade**。

0.1.2 的 LuCI RPC 对象不可用时，升级页本身也打不开，所以请先用上面的
`chmod` 恢复，或手动安装 0.1.3 的 LuCI 包：

```sh
cd /tmp
BASE=https://github.com/moliango/rust-dns/releases/download/v0.1.3
wget "$BASE/24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.3-r1_all.ipk"
opkg install ./24.10.8_x86_64-24.10.8_luci-app-rust-dns_0.1.3-r1_all.ipk
```

## 五、验证

- `luci-app-rust-dns` ipk 内 `/usr/share/rpcd/ucode/luci.rust_dns` 权限为 `0644`。
- 安装后 `ubus -v list luci.rust_dns` 能看到 `get_stats`、`upgrade_check` 等方法。
- 统计页读取 `/api/v1/stats`、`/api/v1/history` 正常。
- 其余功能与 0.1.2 相同；0.1.1 的 21 项修复和 GitHub 升级功能保持不变。
