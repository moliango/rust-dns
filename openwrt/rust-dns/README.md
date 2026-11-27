# rust-dns OpenWrt service package

Packages the statically linked `rust-dns` daemon together with its procd init
script, UCI schema, UCI -> JSON generator, dnsmasq/firewall4 integration and
geodata updater.  The LuCI frontend lives in the separate `luci-app-rust-dns`
package and depends on this one.

License: GPL-3.0-only.  The package reuses shell/ucode structure from the
reference `luci-app-mosdns` integration; keep the SPDX headers and attribution.

## Layout

```
openwrt/rust-dns/
├── Makefile                 # prebuilt static binary package
├── hashes/<arch>.sha256     # CI-generated SHA256 per release artifact
├── files/
│   ├── rust-dns.config      # /etc/config/rust-dns (conffile)
│   ├── config_custom.yaml   # /etc/rust-dns/config_custom.yaml (conffile)
│   ├── rule/*.txt           # 9 rule files (conffiles)
│   ├── rust-dns.init        # /etc/init.d/rust-dns
│   ├── config.sh            # /usr/share/rust-dns/config.sh
│   ├── rust-dns.uc          # /usr/share/rust-dns/rust-dns.uc
│   ├── nft-hijack.nft       # firewall4 template
│   ├── dnsmasq-rust-dns.conf# dnsmasq template
│   ├── hotplug-iface        # /etc/hotplug.d/iface/95-rust-dns
│   └── uci-defaults/90-rust-dns
├── dev/stub/rust-dns        # local CLI stub, never installed
└── tests/config_generator/  # host-side golden tests
```

Fixed runtime paths:

| Path | Purpose |
|---|---|
| `/var/etc/rust-dns/config.json` | generated, validated runtime config |
| `/var/etc/rust-dns/config.json.last-good` | last config that survived 5s |
| `/var/etc/rust-dns/last-error.log` | latest check/start error (LuCI reads it) |
| `/var/etc/rust-dns/.rollback-attempt` | failed-config hash, one rollback guard |
| `/var/rust-dns/` | geosite/geoip/adlist, cache/stats dumps, updater state |
| `/var/log/rust-dns.log` | daemon log (rotation by the core binary) |

## Release artifacts

CI builds the Rust 1.100.0 toolchain with MSRV 1.88 and publishes, for each
target, a tarball with one top-level directory `rust-dns-<version>-<arch>/`
containing `rust-dns`:

```
rust-dns-0.1.4-x86_64.tar.gz
rust-dns-0.1.4-aarch64.tar.gz
rust-dns-0.1.4-armv7.tar.gz
```

The Makefile maps OpenWrt `ARCH` to `x86_64`, `aarch64` or `armv7`, reads
`hashes/<arch>.sha256` as `PKG_HASH`, and refuses other architectures (v1 does
not support MIPS/armv5).  The placeholder hashes in `hashes/` must be replaced
by CI before a real build.  The release repository defaults to
`moliango/rust-dns`; override it with `RUST_DNS_REPO=owner/repo`.

## Build (OpenWrt SDK / buildroot)

```sh
# from an OpenWrt buildroot (23.05 or 24.10, opkg) or 25.12/main (apk)
make package/rust-dns/compile V=s
ls bin/targets/*/*/packages/ | grep rust-dns
```

The same Makefile and conffiles definition are used by both backends:

```sh
# opkg (23.05, 24.10)
opkg install rust-dns_0.1.4-*.ipk

# apk (25.12/main)
apk add --allow-untrusted rust-dns-0.1.4-*.apk
```

The package does **not** enable the service automatically.  After installing:

```sh
/etc/init.d/rust-dns enable
/etc/init.d/rust-dns start
ubus call service list '{"name":"rust-dns"}'
dig @127.0.0.1 -p 5335 example.com
```

## UCI -> JSON generator

`files/config.sh` renders `/var/etc/rust-dns/config.json`:

```sh
sh /usr/share/rust-dns/config.sh render
```

It reads UCI with `config_load` / `config_get` / `config_list_foreach`, emits a
deterministic JSON document (top-level `log`, `api`, `include`, `plugins`) and
never writes files itself.  The init script redirects stdout to
`config.json.tmp`, runs `rust-dns check -c`, then renames atomically.

Cross-task contracts (do not rename without updating LuCI and the stats API):

- `api.http = <api_listen_address>:<listen_port_api>`; v1 only allows
  `127.0.0.1` or `::1` (non-loopback values are clamped with a warning).
- `plugins[tag=stats_collector].args` contains `enabled`, `capacity`,
  `dump_file`, `dump_interval`, `history_max`, `top_max`, `logs_max`.
- Rule files are exposed as `domain_set`/`ip_set` plugins with the fixed
  `/etc/rust-dns/rule/*.txt` paths.

Setting `option configfile '/etc/rust-dns/config_custom.yaml'` bypasses the
generator: the custom YAML is copied to `config.json.tmp` and takes the same
`check` -> atomic-rename -> watchdog path.

## Tests (Linux host, no OpenWrt required)

```sh
cd openwrt/rust-dns
sh tests/config_generator/run.sh          # golden diff + structural assertions
sh tests/config_generator/run.sh --update # regenerate fixtures/json/*.json
```

The runner:

1. runs `sh -n` over the init script, `config.sh`, hotplug, uci-defaults, the
   stub and itself;
2. compiles the Python helpers;
3. renders every `tests/config_generator/fixtures/uci/*.uci` through a fake
   `config_load`/`config_get`/`config_list_foreach` harness and diffs the
   output against `fixtures/json/`;
4. asserts `api.http`, stats args, server entries and all nine rule paths;
5. checks that `fixtures/invalid/*.uci` fail with a readable error;
6. smoke-tests `dev/stub/rust-dns version|check|start`, including the
   missing-argument error path under `set -u`;
7. optionally validates every golden fixture with the real daemon when
   `RUST_DNS_BIN` points at a runnable binary:

```sh
RUST_DNS_BIN=../../rust-dns/target/aarch64-unknown-linux-musl/release/rust-dns \
  sh tests/config_generator/run.sh
```

`fixtures/uci/defaults.uci` is a copy of the shipped `files/rust-dns.config`,
so the golden test also proves the default UCI schema renders.

Requires `python3` (standard library only) and `sh`; no jq needed.

## Local stub

`dev/stub/rust-dns` implements the CLI contract used by the init script:

```sh
dev/stub/rust-dns version
dev/stub/rust-dns check -c /var/etc/rust-dns/config.json
dev/stub/rust-dns start -c /var/etc/rust-dns/config.json -d /var/rust-dns
```

- `check` parses JSON, or YAML for the custom-config mode, and validates
  loopback `api.http`, server ports and the stats keys for generated JSON
  (falls back to a structural check when `python3`/PyYAML is absent).
- `RUST_DNS_STUB_FAIL_AFTER=<seconds>` makes `start` exit non-zero after the
  delay, which is how the init watchdog / last-good rollback path is exercised.
- `RUST_DNS_STUB_ONCE=1` exits cleanly after one second.

The stub is not installed by the package.  The repository-root
`dev/stub/rust-dns` is a thin wrapper around this canonical copy so LuCI
development can use the same CLI from the existing `dev/stub` tree.

## Updater (`rust-dns.uc`)

`ucode /usr/share/rust-dns/rust-dns.uc update --all|--geosite|--geoip|--adlist`
takes an atomic `mkdir` lock (exit 2 while another update runs), downloads and
converts into `/var/rust-dns/.staging.<pid>`, validates non-empty output, then
replaces the live files one at a time keeping one generation in `.bak/`.  A
failed commit or manifest write restores every file it already replaced, so a
failed update never leaves a mix of old and new data.  `file://` source URLs
are copied directly (used by offline fixtures); `https://` sources go through
`uclient-fetch`, optionally behind `github_proxy`.

`/var/rust-dns/manifest.json` exposes `geosite` / `geoip` / `adlist` summaries
(`sha256`, `count`, `bytes` for the primary `*_cn.txt` / `adlist.txt`) plus a
`files` map with every generated file.  Each run appends
`UPDATE_EXITED <code>` to `/var/rust-dns/update.log`.

`ucode /usr/share/rust-dns/rust-dns.uc upgrade --check` prints the latest
GitHub release state as JSON and `upgrade` downloads the matching ipk/apk set,
verifies it against `SHA256SUMS-packages`, installs it and appends
`UPGRADE_EXITED <code>` to `/var/rust-dns/upgrade.log`.  The LuCI System
Upgrade page drives the same commands.

## Validation notes

This repository checkout does not provide `ucode`, `shellcheck` or an OpenWrt
SDK, so those checks could not be executed here.  The completed checks are:

- `sh -n` on every shell script (via the test runner);
- golden UCI -> JSON comparisons and structural assertions;
- `python3 -m json.tool` on every generated fixture;
- stub `version`, `check` and `start` smoke tests.

Before release, still run on-target:

```sh
shellcheck files/rust-dns.init files/config.sh
ucode -c files/rust-dns.uc
```

and the three-SDK integration matrix (23.05 opkg, 24.10 opkg, 25.12 apk).
