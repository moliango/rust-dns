#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
#
# rust-dns UCI -> JSON configuration generator.
#
# The generator owns the UCI -> runtime-config contract described in
# .trellis/tasks/11-23-rust-dns-service/design.md sections 3 and 4.  It reads
# /etc/config/rust-dns through the standard OpenWrt shell helpers
# (config_load / config_get / config_list_foreach) and prints a deterministic
# JSON document (JSON is a subset of YAML, so the core binary can parse it with
# a YAML parser).
#
# Usage:
#     /usr/share/rust-dns/config.sh render
#
# On OpenWrt the helpers come from /lib/functions.sh, which this script loads
# when it is executed standalone.  The golden tests under
# ../tests/config_generator provide their own implementations of the helpers
# before sourcing this file, which keeps the renderer testable off-device.

CONFIG_PKG="rust-dns"
CONFIG_SECTION="config"

RD_GENERATED_CONFIG="/var/etc/rust-dns/config.json"
RD_CUSTOM_CONFIG="/etc/rust-dns/config_custom.yaml"
RD_CACHE_DUMP="/var/rust-dns/cache.dump"
RD_RULE_DIR="/etc/rust-dns/rule"
RD_GEOSITE_FILE="/var/rust-dns/geosite_cn.txt"
RD_GEOIP_FILE="/var/rust-dns/geoip_cn.txt"
RD_ADLIST_FILE="/var/rust-dns/adlist.txt"

# ---------------------------------------------------------------------------
# Platform helpers
# ---------------------------------------------------------------------------

if ! command -v config_get >/dev/null 2>&1; then
    if [ -r /lib/functions.sh ]; then
        . /lib/functions.sh
    fi
fi

# ---------------------------------------------------------------------------
# Small helpers
# ---------------------------------------------------------------------------

_rd_warn() {
    printf 'rust-dns: config: warning: %s\n' "$*" >&2
}

_rd_error() {
    printf 'rust-dns: config: %s\n' "$*" >&2
}

_rd_option() {
    # _rd_option <name> <default>; prints the value on stdout.
    _rd_o=""
    config_get _rd_o "$CONFIG_SECTION" "$1" "$2"
    printf '%s' "$_rd_o"
}

_rd_is_true() {
    case "$1" in
        1|on|true|yes|enabled) return 0 ;;
        *) return 1 ;;
    esac
}

_rd_bool() {
    if _rd_is_true "$1"; then
        printf 'true'
    else
        printf 'false'
    fi
}

# _rd_uint_strict <value> <min> <max> <name>
# Sets _RD_NUM on success; sets _RD_ERR and returns 1 on failure.
_rd_uint_strict() {
    case "$1" in
        ''|*[!0-9]*)
            _RD_ERR="$4 must be an integer, got '$1'"
            return 1
            ;;
    esac
    if [ "$1" -lt "$2" ] || [ "$1" -gt "$3" ]; then
        _RD_ERR="$4 must be between $2 and $3, got $1"
        return 1
    fi
    _RD_NUM=$1
    return 0
}

# _rd_uint_clamp <value> <default> <min> <max>
# Non-integers fall back to <default>; values are clamped into [min, max].
_rd_uint_clamp() {
    case "$1" in
        ''|*[!0-9]*)
            _RD_NUM=$2
            return 0
            ;;
    esac
    if [ "$1" -lt "$3" ]; then
        _RD_NUM=$3
    elif [ "$1" -gt "$4" ]; then
        _RD_NUM=$4
    else
        _RD_NUM=$1
    fi
    return 0
}

_rd_list() {
    # _rd_list <option>; prints one value per line.  Falls back to a scalar
    # option when the configuration uses "option" instead of "list".
    _RD_LIST=""
    _rd_list_collect() {
        _RD_LIST="${_RD_LIST}${1}
"
    }
    config_list_foreach "$CONFIG_SECTION" "$1" _rd_list_collect
    if [ -z "$_RD_LIST" ]; then
        _rd_scalar="$(_rd_option "$1" '')"
        if [ -n "$_rd_scalar" ]; then
            _RD_LIST="$_rd_scalar
"
        fi
    fi
    printf '%s' "$_RD_LIST"
}

_rd_json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/	/\\t/g' -e 's/\r/\\r/g'
}

_rd_jstr() {
    printf '"%s"' "$(_rd_json_escape "$1")"
}

_rd_upstream() {
    case "$1" in
        *://*) printf '%s' "$1" ;;
        *) printf 'udp://%s' "$1" ;;
    esac
}

_rd_is_tls() {
    case "$1" in
        tls://*|https://*|quic://*|h3://*) return 0 ;;
        *) return 1 ;;
    esac
}

_rd_listen() {
    # _rd_listen <address> <port>
    case "$1" in
        *:*)
            case "$1" in
                \[*\]) printf '%s:%s' "$1" "$2" ;;
                *) printf '[%s]:%s' "$1" "$2" ;;
            esac
            ;;
        *)
            printf '%s:%s' "$1" "$2"
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Plugin emission (all functions run in the caller's shell so the comma
# counter survives; never call them via command substitution)
# ---------------------------------------------------------------------------

_RD_PLUGINS=0

_rd_begin_plugin() {
    if [ "$_RD_PLUGINS" -gt 0 ]; then
        printf ',\n'
    fi
    _RD_PLUGINS=$((_RD_PLUGINS + 1))
}

_rd_emit_set() {
    # _rd_emit_set <type> <tag> <file>
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": %s,\n' "$(_rd_jstr "$2")"
    printf '      "type": %s,\n' "$(_rd_jstr "$1")"
    printf '      "args": {\n'
    printf '        "files": [%s]\n' "$(_rd_jstr "$3")"
    printf '      }\n'
    printf '    }'
}

_rd_emit_stats() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "stats_collector",\n'
    printf '      "type": "stats_collector",\n'
    printf '      "args": {\n'
    printf '        "enabled": %s,\n' "$(_rd_bool "$stats_collector")"
    printf '        "capacity": %s,\n' "$stats_capacity"
    printf '        "dump_file": %s,\n' "$(_rd_jstr "$stats_dump_path_out")"
    printf '        "dump_interval": %s,\n' "$stats_dump_interval"
    printf '        "history_max": %s,\n' "$stats_history_max"
    printf '        "top_max": %s,\n' "$stats_top_max"
    printf '        "logs_max": %s\n' "$stats_logs_max"
    printf '      }\n'
    printf '    }'
}

_rd_emit_bootstrap() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "bootstrap",\n'
    printf '      "type": "bootstrap",\n'
    printf '      "args": {\n'
    printf '        "servers": [%s]\n' "$(_rd_jstr "$bootstrap_dns")"
    printf '      }\n'
    printf '    }'
}

_rd_emit_cache() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "cache",\n'
    printf '      "type": "cache",\n'
    printf '      "args": {\n'
    printf '        "size": %s,\n' "$cache_size"
    printf '        "lazy_cache_ttl": %s,\n' "$lazy_cache_ttl"
    printf '        "dump_file": %s,\n' "$(_rd_jstr "$RD_CACHE_DUMP")"
    printf '        "dump_interval": %s,\n' "$dump_interval"
    printf '        "prefetch": %s,\n' "$(_rd_bool "$prefetch")"
    printf '        "prefetch_before_expire": %s,\n' "$prefetch_before_expire"
    printf '        "prefetch_min_hits": %s,\n' "$prefetch_min_hits"
    printf '        "prefetch_scan_interval": %s\n' "$prefetch_scan_interval"
    printf '      }\n'
    printf '    }'
}

_rd_emit_forward() {
    # _rd_emit_forward <tag> <newline separated upstreams>
    _rd_tag=$1
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": %s,\n' "$(_rd_jstr "$_rd_tag")"
    printf '      "type": "forward",\n'
    printf '      "args": {\n'
    printf '        "upstreams": [\n'
    _rd_first=1
    while IFS= read -r _rd_u; do
        [ -n "$_rd_u" ] || continue
        if [ "$_rd_first" -eq 0 ]; then
            printf ',\n'
        fi
        _rd_first=0
        printf '          {\n'
        printf '            "addr": %s' "$(_rd_jstr "$(_rd_upstream "$_rd_u")")"
        if _rd_is_tls "$_rd_u"; then
            printf ',\n            "enable_pipeline": %s' "$(_rd_bool "$enable_pipeline")"
            printf ',\n            "verify": %s' "$(_rd_bool "$tls_verify")"
        fi
        printf '\n          }'
    done <<EOF
$2
EOF
    printf '\n        ],\n'
    printf '        "concurrent": %s,\n' "$concurrent"
    printf '        "idle_timeout": %s,\n' "$idle_timeout"
    printf '        "ecs": %s' "$(_rd_bool "$enable_ecs_remote")"
    if _rd_is_true "$enable_ecs_remote" && [ -n "$remote_ecs_ip" ]; then
        printf ',\n        "ecs_addr": %s' "$(_rd_jstr "$remote_ecs_ip")"
    fi
    if [ "$_rd_tag" = "forward_remote" ]; then
        printf ',\n        "dns_leak": %s' "$(_rd_bool "$dns_leak")"
    fi
    printf '\n      }\n'
    printf '    }'
}

_rd_emit_cloudflare_ips() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "cloudflare_ip",\n'
    printf '      "type": "ip_set",\n'
    printf '      "args": {\n'
    printf '        "ips": [\n'
    _rd_first=1
    while IFS= read -r _rd_ip; do
        [ -n "$_rd_ip" ] || continue
        if [ "$_rd_first" -eq 0 ]; then
            printf ',\n'
        fi
        _rd_first=0
        printf '          %s' "$(_rd_jstr "$_rd_ip")"
    done <<EOF
$cloudflare_ips
EOF
    printf '\n        ]\n'
    printf '      }\n'
    printf '    }'
}

_rd_emit_modifier() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "modify_response",\n'
    printf '      "type": "response_modifier",\n'
    printf '      "args": {\n'
    printf '        "minimal_ttl": %s,\n' "$minimal_ttl"
    printf '        "maximum_ttl": %s,\n' "$maximum_ttl"
    printf '        "prefer_ipv4_cn": %s,\n' "$(_rd_bool "$prefer_ipv4_cn")"
    printf '        "prefer_ipv4": %s,\n' "$(_rd_bool "$prefer_ipv4")"
    printf '        "reject_type65": %s\n' "$(_rd_bool "$reject_type65")"
    printf '      }\n'
    printf '    }'
}

_rd_emit_sequence() {
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": "main",\n'
    printf '      "type": "sequence",\n'
    printf '      "args": {\n'
    printf '        "rules": [\n'
    _rd_first=1
    while IFS= read -r _rd_rule; do
        [ -n "$_rd_rule" ] || continue
        if [ "$_rd_first" -eq 0 ]; then
            printf ',\n'
        fi
        _rd_first=0
        printf '          %s' "$(_rd_jstr "$_rd_rule")"
    done <<EOF
$sequence_rules
EOF
    printf '\n        ]\n'
    printf '      }\n'
    printf '    }'
}

_rd_emit_server() {
    # _rd_emit_server <tag> <type> <listen>
    _rd_begin_plugin
    printf '    {\n'
    printf '      "tag": %s,\n' "$(_rd_jstr "$1")"
    printf '      "type": %s,\n' "$(_rd_jstr "$2")"
    printf '      "args": {\n'
    printf '        "entry": "main",\n'
    printf '        "listen": %s\n' "$(_rd_jstr "$3")"
    printf '      }\n'
    printf '    }'
}

# ---------------------------------------------------------------------------
# Renderer
# ---------------------------------------------------------------------------

rust_dns_render() {
    if ! command -v config_load >/dev/null 2>&1; then
        _rd_error "config_load is unavailable; source /lib/functions.sh first"
        return 1
    fi
    if ! command -v config_get >/dev/null 2>&1; then
        _rd_error "config_get is unavailable; source /lib/functions.sh first"
        return 1
    fi

    config_load "$CONFIG_PKG" || {
        _rd_error "failed to load UCI package '$CONFIG_PKG'"
        return 1
    }

    # --- raw values -------------------------------------------------------
    enabled="$(_rd_option enabled 0)"
    configfile="$(_rd_option configfile "$RD_GENERATED_CONFIG")"
    listen_address="$(_rd_option listen_address 0.0.0.0)"
    listen_port_raw="$(_rd_option listen_port 5335)"
    log_level="$(_rd_option log_level error)"
    log_file="$(_rd_option log_file /var/log/rust-dns.log)"
    log_size_raw="$(_rd_option log_size 1)"
    log_backups_raw="$(_rd_option log_backups 2)"
    cache="$(_rd_option cache 1)"
    cache_size_raw="$(_rd_option cache_size 8000)"
    lazy_cache_ttl_raw="$(_rd_option lazy_cache_ttl 86400)"
    dump_interval_raw="$(_rd_option dump_interval 3600)"
    prefetch="$(_rd_option prefetch 0)"
    prefetch_before_expire_raw="$(_rd_option prefetch_before_expire 10)"
    prefetch_min_hits_raw="$(_rd_option prefetch_min_hits 3)"
    prefetch_scan_interval_raw="$(_rd_option prefetch_scan_interval 5)"
    concurrent_raw="$(_rd_option concurrent 2)"
    idle_timeout_raw="$(_rd_option idle_timeout 30)"
    enable_pipeline="$(_rd_option enable_pipeline 1)"
    insecure_skip_verify="$(_rd_option insecure_skip_verify 0)"
    bootstrap_dns="$(_rd_option bootstrap_dns 119.29.29.29)"
    api_port_raw="$(_rd_option listen_port_api 9091)"
    api_listen_address="$(_rd_option api_listen_address 127.0.0.1)"
    stats_collector="$(_rd_option stats_collector 1)"
    stats_capacity_raw="$(_rd_option stats_capacity 2000)"
    stats_dump_file="$(_rd_option stats_dump_file 0)"
    stats_dump_interval_raw="$(_rd_option stats_dump_interval 600)"
    stats_dump_path="$(_rd_option stats_dump_path /var/rust-dns/stats.dump)"
    stats_history_max_raw="$(_rd_option stats_history_max 168)"
    stats_top_max_raw="$(_rd_option stats_top_max 100)"
    stats_logs_max_raw="$(_rd_option stats_logs_max 200)"
    minimal_ttl_raw="$(_rd_option minimal_ttl 0)"
    maximum_ttl_raw="$(_rd_option maximum_ttl 0)"
    prefer_ipv4_cn="$(_rd_option prefer_ipv4_cn 0)"
    prefer_ipv4="$(_rd_option prefer_ipv4 0)"
    reject_type65="$(_rd_option reject_type65 0)"
    dns_leak="$(_rd_option dns_leak 0)"
    enable_ecs_remote="$(_rd_option enable_ecs_remote 0)"
    remote_ecs_ip="$(_rd_option remote_ecs_ip '')"
    adblock="$(_rd_option adblock 0)"
    cloudflare="$(_rd_option cloudflare 0)"
    cloudflare_ips="$(_rd_list cloudflare_ip)"
    remote_dns="$(_rd_list remote_dns)"
    local_dns="$(_rd_list local_dns)"
    stream_media_dns="$(_rd_list stream_media_dns)"

    # --- validation -------------------------------------------------------
    if [ "$configfile" != "$RD_GENERATED_CONFIG" ] && [ "$configfile" != "$RD_CUSTOM_CONFIG" ]; then
        _rd_error "configfile must be '$RD_GENERATED_CONFIG' or '$RD_CUSTOM_CONFIG' (got '$configfile')"
        return 1
    fi

    if ! _rd_uint_strict "$listen_port_raw" 1 65535 listen_port; then
        _rd_error "$_RD_ERR"
        return 1
    fi
    listen_port=$_RD_NUM

    if ! _rd_uint_strict "$api_port_raw" 1 65535 listen_port_api; then
        _rd_error "$_RD_ERR"
        return 1
    fi
    api_port=$_RD_NUM

    if [ -z "$listen_address" ]; then
        _rd_error "listen_address must not be empty"
        return 1
    fi

    case "$api_listen_address" in
        127.0.0.1|::1) ;;
        *)
            _rd_warn "api_listen_address '$api_listen_address' is not loopback; using 127.0.0.1"
            api_listen_address=127.0.0.1
            ;;
    esac

    if [ -z "$remote_dns" ]; then
        _rd_error "remote_dns must not be empty"
        return 1
    fi

    # --- clamping ---------------------------------------------------------
    _rd_uint_clamp "$stats_capacity_raw" 2000 100 100000
    stats_capacity=$_RD_NUM
    _rd_uint_clamp "$stats_dump_interval_raw" 600 60 86400
    stats_dump_interval=$_RD_NUM
    _rd_uint_clamp "$stats_history_max_raw" 168 1 168
    stats_history_max=$_RD_NUM
    _rd_uint_clamp "$stats_top_max_raw" 100 1 1000
    stats_top_max=$_RD_NUM
    _rd_uint_clamp "$stats_logs_max_raw" 200 1 "$stats_capacity"
    stats_logs_max=$_RD_NUM
    # _rd_uint_clamp returns its default untouched for non-integers, so the
    # default (200) can still exceed a small stats_capacity.  The API contract
    # requires logs_max <= capacity.
    [ "$stats_logs_max" -le "$stats_capacity" ] || stats_logs_max=$stats_capacity
    _rd_uint_clamp "$log_size_raw" 1 0 1024
    log_size=$_RD_NUM
    _rd_uint_clamp "$log_backups_raw" 2 0 9
    log_backups=$_RD_NUM
    _rd_uint_clamp "$cache_size_raw" 8000 0 100000000
    cache_size=$_RD_NUM
    _rd_uint_clamp "$lazy_cache_ttl_raw" 86400 0 31536000
    lazy_cache_ttl=$_RD_NUM
    _rd_uint_clamp "$dump_interval_raw" 3600 0 86400
    dump_interval=$_RD_NUM
    _rd_uint_clamp "$prefetch_before_expire_raw" 10 0 86400
    prefetch_before_expire=$_RD_NUM
    _rd_uint_clamp "$prefetch_min_hits_raw" 3 1 1000
    prefetch_min_hits=$_RD_NUM
    _rd_uint_clamp "$prefetch_scan_interval_raw" 5 1 3600
    prefetch_scan_interval=$_RD_NUM
    _rd_uint_clamp "$concurrent_raw" 2 1 1024
    concurrent=$_RD_NUM
    _rd_uint_clamp "$idle_timeout_raw" 30 0 3600
    idle_timeout=$_RD_NUM
    _rd_uint_clamp "$minimal_ttl_raw" 0 0 604800
    minimal_ttl=$_RD_NUM
    _rd_uint_clamp "$maximum_ttl_raw" 0 0 604800
    maximum_ttl=$_RD_NUM

    if _rd_is_true "$insecure_skip_verify"; then
        tls_verify=0
    else
        tls_verify=1
    fi

    # Resolve the fallback upstreams once so the output stays deterministic.
    if [ -z "$local_dns" ]; then
        local_dns="$bootstrap_dns
"
    fi
    if [ -z "$stream_media_dns" ]; then
        stream_media_dns="$remote_dns"
    fi

    if _rd_is_true "$stats_dump_file"; then
        stats_dump_path_out="$stats_dump_path"
    else
        stats_dump_path_out=""
    fi

    api_http="$(_rd_listen "$api_listen_address" "$api_port")"
    server_listen="$(_rd_listen "$listen_address" "$listen_port")"

    # --- sequence ---------------------------------------------------------
    sequence_rules=""
    _rd_add_rule() {
        sequence_rules="${sequence_rules}${1}
"
    }
    _rd_add_rule "hosts"
    _rd_add_rule "whitelist -> forward_local"
    _rd_add_rule "blocklist -> reject"
    if _rd_is_true "$adblock"; then
        _rd_add_rule "adlist -> reject"
    fi
    _rd_add_rule "local-ptr -> reject"
    _rd_add_rule "ddnslist -> forward_local"
    _rd_add_rule "streaming -> forward_stream"
    _rd_add_rule "geosite_cn -> forward_local"
    _rd_add_rule "geoip_cn -> forward_local"
    _rd_add_rule "greylist -> forward_remote"
    if _rd_is_true "$cloudflare"; then
        _rd_add_rule "cloudflare_cidr -> forward_remote"
    fi
    _rd_add_rule "redirect"
    _rd_add_rule "modify_response"
    if _rd_is_true "$cache"; then
        _rd_add_rule "cache"
    fi
    _rd_add_rule "forward_remote"

    # --- output -----------------------------------------------------------
    _RD_PLUGINS=0
    printf '{\n'
    printf '  "log": {\n'
    printf '    "level": %s,\n' "$(_rd_jstr "$log_level")"
    printf '    "file": %s,\n' "$(_rd_jstr "$log_file")"
    printf '    "size": %s,\n' "$(_rd_jstr "$log_size")"
    printf '    "backups": %s\n' "$log_backups"
    printf '  },\n'
    printf '  "api": {\n'
    printf '    "http": %s\n' "$(_rd_jstr "$api_http")"
    printf '  },\n'
    printf '  "include": [],\n'
    printf '  "plugins": [\n'

    _rd_emit_stats
    if [ -n "$bootstrap_dns" ]; then
        _rd_emit_bootstrap
    fi
    _rd_emit_set domain_set geosite_cn "$RD_GEOSITE_FILE"
    _rd_emit_set ip_set geoip_cn "$RD_GEOIP_FILE"
    _rd_emit_set domain_set adlist "$RD_ADLIST_FILE"
    _rd_emit_set domain_set hosts "$RD_RULE_DIR/hosts.txt"
    _rd_emit_set domain_set whitelist "$RD_RULE_DIR/whitelist.txt"
    _rd_emit_set domain_set blocklist "$RD_RULE_DIR/blocklist.txt"
    _rd_emit_set domain_set greylist "$RD_RULE_DIR/greylist.txt"
    _rd_emit_set domain_set ddnslist "$RD_RULE_DIR/ddnslist.txt"
    _rd_emit_set domain_set redirect "$RD_RULE_DIR/redirect.txt"
    _rd_emit_set domain_set local-ptr "$RD_RULE_DIR/local-ptr.txt"
    _rd_emit_set domain_set streaming "$RD_RULE_DIR/streaming.txt"
    _rd_emit_set ip_set cloudflare_cidr "$RD_RULE_DIR/cloudflare-cidr.txt"
    if [ -n "$cloudflare_ips" ]; then
        _rd_emit_cloudflare_ips
    fi
    if _rd_is_true "$cache"; then
        _rd_emit_cache
    fi
    _rd_emit_forward forward_local "$local_dns"
    _rd_emit_forward forward_stream "$stream_media_dns"
    _rd_emit_forward forward_remote "$remote_dns"
    _rd_emit_modifier
    _rd_emit_sequence
    _rd_emit_server dns_udp udp_server "$server_listen"
    _rd_emit_server dns_tcp tcp_server "$server_listen"

    printf '\n  ]\n'
    printf '}\n'
    return 0
}

# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------

rust_dns_config_usage() {
    cat <<'EOF'
Usage: config.sh render

Render /etc/config/rust-dns into a deterministic JSON document on stdout.
The output is validated by the init script with 'rust-dns check' before it is
installed as /var/etc/rust-dns/config.json.
EOF
}

rust_dns_config_main() {
    case "${1:-render}" in
        render)
            rust_dns_render
            ;;
        -h|--help|help)
            rust_dns_config_usage
            return 0
            ;;
        *)
            rust_dns_config_usage >&2
            return 1
            ;;
    esac
}

# Run only when executed directly.  The init script, the test harness and any
# caller that wants to reuse the helpers can source this file instead.
if [ "${RUST_DNS_CONFIG_SH_LIB:-0}" != "1" ]; then
    case "${0##*/}" in
        config.sh)
            rust_dns_config_main "$@"
            exit $?
            ;;
    esac
fi
