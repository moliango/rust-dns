#!/usr/bin/env python3
"""Structural assertions for generated rust-dns configurations.

The golden diff checks every byte, but these assertions name the cross-task
contract explicitly: api.http, the stats_collector args, server entries and the
installed rule paths.
"""

import json
import re
import sys

RULE_FILES = [
    "/etc/rust-dns/rule/whitelist.txt",
    "/etc/rust-dns/rule/blocklist.txt",
    "/etc/rust-dns/rule/greylist.txt",
    "/etc/rust-dns/rule/ddnslist.txt",
    "/etc/rust-dns/rule/hosts.txt",
    "/etc/rust-dns/rule/redirect.txt",
    "/etc/rust-dns/rule/local-ptr.txt",
    "/etc/rust-dns/rule/streaming.txt",
    "/etc/rust-dns/rule/cloudflare-cidr.txt",
]

STATS_KEYS = [
    "enabled",
    "capacity",
    "dump_file",
    "dump_interval",
    "history_max",
    "top_max",
    "logs_max",
]

EXPECT = {
    "basic": {
        "api": "127.0.0.1:9091",
        "capacity": 2000,
        "dump_interval": 600,
        "history_max": 168,
        "top_max": 100,
        "logs_max": 200,
        "listen": "0.0.0.0:5335",
        "enabled": True,
    },
    "custom": {
        "api": "[::1]:9191",
        "capacity": 100,
        "dump_interval": 60,
        "history_max": 168,
        "top_max": 1000,
        "logs_max": 100,
        "listen": "[::]:5353",
        "enabled": False,
    },
    "api-clamp": {
        "api": "127.0.0.1:9091",
    },
}


def fail(msg: str) -> None:
    print(f"assert_json: {msg}", file=sys.stderr)
    sys.exit(1)


def main() -> int:
    if len(sys.argv) < 2:
        fail("usage: assert_json.py <config.json> [fixture-name]")

    path = sys.argv[1]
    name = sys.argv[2] if len(sys.argv) > 2 else ""

    with open(path, encoding="utf-8") as fh:
        doc = json.load(fh)

    for key in ("log", "api", "include", "plugins"):
        if key not in doc:
            fail(f"missing top-level key {key!r}")

    http = doc["api"].get("http") if isinstance(doc["api"], dict) else None
    if not isinstance(http, str) or not re.fullmatch(r"(\[[0-9a-fA-F:]+\]|[0-9.]+):[0-9]+", http):
        fail(f"invalid api.http: {http!r}")

    plugins = doc["plugins"]
    if not isinstance(plugins, list) or not plugins:
        fail("plugins must be a non-empty list")

    by_tag = {}
    files = set()
    for plugin in plugins:
        if "tag" not in plugin or "type" not in plugin:
            fail(f"plugin without tag/type: {plugin!r}")
        by_tag[plugin["tag"]] = plugin
        for entry in plugin.get("args", {}).get("files", []):
            files.add(entry)

    stats = by_tag.get("stats_collector")
    if stats is None:
        fail("stats_collector plugin is missing")
    for key in STATS_KEYS:
        if key not in stats.get("args", {}):
            fail(f"stats_collector.args.{key} is missing")

    servers = [p for p in plugins if p.get("type") in ("udp_server", "tcp_server")]
    if {p["tag"] for p in servers} != {"dns_udp", "dns_tcp"}:
        fail(f"unexpected server plugins: {[p['tag'] for p in servers]}")
    listens = {p["args"].get("entry") for p in servers}
    if listens != {"main"}:
        fail(f"server plugins must enter 'main', got {listens}")
    main = by_tag.get("main")
    if main is None or main.get("type") != "sequence":
        fail("main sequence plugin is missing")

    missing = [r for r in RULE_FILES if r not in files]
    if missing:
        fail(f"rule files missing from plugins: {missing}")

    expected = EXPECT.get(name)
    if expected:
        for key, value in expected.items():
            if key == "api":
                if http != value:
                    fail(f"{name}: api.http expected {value!r}, got {http!r}")
            elif key == "listen":
                for server in servers:
                    if server["args"].get("listen") != value:
                        fail(f"{name}: listen expected {value!r}, got {server['args'].get('listen')!r}")
            elif key == "enabled":
                if stats["args"].get("enabled") is not value:
                    fail(f"{name}: stats enabled expected {value}, got {stats['args'].get('enabled')}")
            else:
                if stats["args"].get(key) != value:
                    fail(f"{name}: stats {key} expected {value}, got {stats['args'].get(key)}")

    print(f"assert_json: {name or path}: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
