#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-3.0-only
"""Mock rust-dns admin API for LuCI frontend development.

Serves the contract documented in .trellis/tasks/11-23-luci-management-ui/
design.md section 5 using the JSON fixtures in ./fixtures.

The real service will only listen on 127.0.0.1, and so does this mock.

Usage:
    python3 server.py [--bind 127.0.0.1] [--port 9091] [--fixtures DIR] [--verbose]

Endpoints:
    GET  /api/v1/health
    GET  /api/v1/version
    GET  /api/v1/stats
    GET  /api/v1/history?points=24
    GET  /api/v1/top?limit=10
    GET  /api/v1/logs?limit=20&offset=0&search=&filter=all
    POST /api/v1/logs/clear
    POST /api/v1/cache/flush

The query log fixture has 24 entries. /logs/clear is stateful for the life of
the process: after it is called, GET /logs returns an empty result set.
"""

from __future__ import annotations

import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any
from urllib.parse import parse_qs, urlparse

DEFAULT_FIXTURES = Path(__file__).resolve().parent / "fixtures"
API_PREFIX = "/api/v1"

# Mirror the defaults of stats_history_max / stats_top_max / stats_logs_max in
# /etc/config/rust-dns so the mock enforces the same upper bounds as rpcd.
HISTORY_MAX = 168
TOP_MAX = 100
LOGS_MAX = 200

_state_lock = threading.Lock()
_state: dict[str, Any] = {
    "logs_cleared": False,
    "cache_flushes": 0,
    "started_at": time.time(),
}


def load_fixture(fixtures_dir: Path, name: str) -> Any:
    path = fixtures_dir / f"{name}.json"
    with path.open("r", encoding="utf-8") as handle:
        return json.load(handle)


class BadRequest(Exception):
    """Raised when a known query parameter is present but invalid."""


def parse_range(
    raw: str | None,
    default: int,
    minimum: int,
    maximum: int | None,
    name: str,
) -> int:
    if raw is None:
        return default
    try:
        value = int(raw)
    except (TypeError, ValueError):
        raise BadRequest(f"invalid {name}") from None
    if value < minimum or (maximum is not None and value > maximum):
        raise BadRequest(f"{name} out of range")
    return value


def filter_logs(
    fixture: dict[str, Any],
    limit: int,
    offset: int,
    search: str,
    log_filter: str,
) -> dict[str, Any]:
    items = fixture.get("items", [])
    needle = search.lower()

    filtered = []
    for item in items:
        if log_filter == "blocked" and not item.get("is_blocked"):
            continue
        if log_filter == "cached" and not item.get("is_cached"):
            continue
        if needle and needle not in str(item.get("domain", "")).lower():
            continue
        filtered.append(item)

    page = filtered[offset : offset + limit]
    return {"total": len(filtered), "items": page}


class MockApiHandler(BaseHTTPRequestHandler):
    server_version = "rust-dns-mock/0.1"

    def log_message(self, fmt: str, *args: Any) -> None:
        if getattr(self.server, "verbose", False):
            super().log_message(fmt, *args)

    def _send_json(self, status: int, payload: Any) -> None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def _fixtures(self) -> Path:
        return self.server.fixtures_dir  # type: ignore[attr-defined]

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        parsed = urlparse(self.path)
        path = parsed.path.rstrip("/") or "/"
        query = parse_qs(parsed.query)

        try:
            if path == f"{API_PREFIX}/health":
                health = load_fixture(self._fixtures(), "health")
                health["uptime_s"] = int(time.time() - _state["started_at"]) + 86400
                self._send_json(200, health)
            elif path == f"{API_PREFIX}/version":
                self._send_json(200, load_fixture(self._fixtures(), "version"))
            elif path == f"{API_PREFIX}/stats":
                self._send_json(200, load_fixture(self._fixtures(), "stats"))
            elif path == f"{API_PREFIX}/history":
                points = parse_range(
                    query.get("points", [None])[0], 24, 1, HISTORY_MAX, "points"
                )
                history = load_fixture(self._fixtures(), "history")
                all_points = history.get("points", [])
                self._send_json(200, {"points": all_points[-points:]})
            elif path == f"{API_PREFIX}/top":
                limit = parse_range(
                    query.get("limit", [None])[0], 10, 1, TOP_MAX, "limit"
                )
                top = load_fixture(self._fixtures(), "top")
                self._send_json(
                    200,
                    {
                        "top_domains": top.get("top_domains", [])[:limit],
                        "top_blocked": top.get("top_blocked", [])[:limit],
                        "top_clients": top.get("top_clients", [])[:limit],
                    },
                )
            elif path == f"{API_PREFIX}/logs":
                limit = parse_range(
                    query.get("limit", [None])[0], 20, 1, LOGS_MAX, "limit"
                )
                offset = parse_range(
                    query.get("offset", [None])[0], 0, 0, None, "offset"
                )
                search = query.get("search", [""])[0]
                if len(search) > 128:
                    raise BadRequest("search too long")
                log_filter = query.get("filter", ["all"])[0]
                if log_filter not in ("all", "blocked", "cached"):
                    raise BadRequest("invalid filter")

                with _state_lock:
                    cleared = _state["logs_cleared"]

                if cleared:
                    self._send_json(200, {"total": 0, "items": []})
                else:
                    fixture = load_fixture(self._fixtures(), "logs")
                    self._send_json(
                        200,
                        filter_logs(fixture, limit, offset, search, log_filter),
                    )
            else:
                self._send_json(404, {"error": "not found"})
        except BadRequest as exc:
            self._send_json(400, {"error": str(exc)})
        except FileNotFoundError as exc:
            self._send_json(500, {"error": f"missing fixture: {exc.filename}"})
        except (OSError, ValueError) as exc:
            self._send_json(500, {"error": str(exc)})

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        parsed = urlparse(self.path)
        path = parsed.path.rstrip("/") or "/"

        # Drain a (small) body so keep-alive clients do not stall.
        length = int(self.headers.get("Content-Length", "0") or "0")
        if 0 < length <= 65536:
            self.rfile.read(length)

        if path == f"{API_PREFIX}/logs/clear":
            with _state_lock:
                _state["logs_cleared"] = True
            self._send_json(200, {"success": True})
        elif path == f"{API_PREFIX}/cache/flush":
            with _state_lock:
                _state["cache_flushes"] += 1
            self._send_json(200, {"success": True})
        else:
            self._send_json(404, {"error": "not found"})


class MockApiServer(ThreadingHTTPServer):
    allow_reuse_address = True
    daemon_threads = True

    def __init__(self, address: tuple[str, int], fixtures_dir: Path, verbose: bool):
        super().__init__(address, MockApiHandler)
        self.fixtures_dir = fixtures_dir  # type: ignore[attr-defined]
        self.verbose = verbose  # type: ignore[attr-defined]


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Mock rust-dns admin API")
    parser.add_argument("--bind", default="127.0.0.1", help="bind address (default: 127.0.0.1)")
    parser.add_argument("--port", type=int, default=9091, help="listen port (default: 9091)")
    parser.add_argument(
        "--fixtures",
        type=Path,
        default=DEFAULT_FIXTURES,
        help="fixture directory (default: ./fixtures next to server.py)",
    )
    parser.add_argument("--verbose", action="store_true", help="log every request to stderr")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    fixtures_dir = args.fixtures.resolve()

    for name in ("health", "version", "stats", "history", "top", "logs"):
        if not (fixtures_dir / f"{name}.json").is_file():
            print(f"error: missing fixture {fixtures_dir / (name + '.json')}")
            return 1

    server = MockApiServer((args.bind, args.port), fixtures_dir, args.verbose)
    print(f"rust-dns mock API listening on http://{args.bind}:{args.port}{API_PREFIX}")
    print(f"fixtures: {fixtures_dir}")
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
