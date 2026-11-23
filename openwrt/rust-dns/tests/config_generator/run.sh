#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-only
#
# Golden / structural tests for /usr/share/rust-dns/config.sh.
#
#   sh run.sh            verify fixtures against fixtures/json/*.json
#   sh run.sh --update   regenerate the golden JSON files (review the diff!)
#
# The renderer normally reads UCI through /lib/functions.sh.  This script
# provides config_load / config_get / config_list_foreach backed by harness.py
# so the exact same code runs on a Linux host without OpenWrt or ucode.

set -u

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
PKG_DIR=$(CDPATH= cd -- "$HERE/../.." && pwd)
CONFIG_SH="$PKG_DIR/files/config.sh"
STUB="$PKG_DIR/dev/stub/rust-dns"

UPDATE=0
[ "${1:-}" = "--update" ] && UPDATE=1

RUN_TMP=$(mktemp -d "${TMPDIR:-/tmp}/rust-dns-golden.XXXXXX")
trap 'rm -rf "$RUN_TMP"' EXIT HUP INT TERM
export RUST_DNS_TEST_TMP="$RUN_TMP"
export RUST_DNS_CONFIG_SH_LIB=1
export PYTHONPYCACHEPREFIX="$RUN_TMP/pycache"

FAILURES=0

fail() {
    printf 'FAIL: %s\n' "$*" >&2
    FAILURES=$((FAILURES + 1))
}

# ---------------------------------------------------------------------------
# UCI helper harness
# ---------------------------------------------------------------------------

config_load() {
    [ -n "${_rd_cfg_dir:-}" ] && rm -rf "$_rd_cfg_dir"
    _rd_cfg_dir=$(mktemp -d "$RUST_DNS_TEST_TMP/uci.XXXXXX") || return 1
    python3 "$HERE/harness.py" "$RUST_DNS_TEST_UCI_FILE" "$_rd_cfg_dir"
}

config_get() {
    # config_get <var> <section> <option> [default]
    _rd_name=$1
    _rd_section=$2
    _rd_option=$3
    _rd_default=${4-}

    if [ -f "$_rd_cfg_dir/$_rd_section/$_rd_option" ]; then
        _rd_value=""
        IFS= read -r _rd_value < "$_rd_cfg_dir/$_rd_section/$_rd_option" || :
    else
        _rd_value=$_rd_default
    fi
    export "$_rd_name=$_rd_value"
}

config_list_foreach() {
    # config_list_foreach <section> <option> <callback> [args...]
    _rd_section=$1
    _rd_option=$2
    _rd_cb=$3
    shift 3
    _rd_file="$_rd_cfg_dir/$_rd_section/$_rd_option.list"
    [ -f "$_rd_file" ] || return 0
    while IFS= read -r _rd_item; do
        "$_rd_cb" "$_rd_item" "$@"
    done < "$_rd_file"
}

RUST_DNS_TEST_UCI_FILE=""
export RUST_DNS_TEST_UCI_FILE

. "$CONFIG_SH"

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

lint_scripts() {
    for _f in \
        "$CONFIG_SH" \
        "$PKG_DIR/files/rust-dns.init" \
        "$PKG_DIR/files/hotplug-iface" \
        "$PKG_DIR/files/uci-defaults/90-rust-dns" \
        "$STUB" \
        "$HERE/run.sh"
    do
        _short=${_f#"$PKG_DIR"/}
        sh -n "$_f" || fail "sh -n $_short"
    done
    python3 -m py_compile "$HERE/harness.py" "$HERE/assert_json.py" || fail "python3 compile"
}

run_valid_fixture() {
    _fixture=$1
    _name=$(basename "$_fixture" .uci)
    _out="$RUN_TMP/$_name.json"
    _err="$RUN_TMP/$_name.err"
    _expected="$HERE/fixtures/json/$_name.json"

    RUST_DNS_TEST_UCI_FILE=$_fixture
    export RUST_DNS_TEST_UCI_FILE

    if ! ( rust_dns_render ) > "$_out" 2> "$_err"; then
        fail "$_name: renderer exited non-zero: $(cat "$_err")"
        return
    fi

    python3 -m json.tool "$_out" > /dev/null || fail "$_name: invalid JSON"
    python3 "$HERE/assert_json.py" "$_out" "$_name" || fail "$_name: structural assertions"

    if [ "$UPDATE" -eq 1 ]; then
        cp "$_out" "$_expected"
        printf 'updated %s\n' "${_expected#"$PKG_DIR"/}"
    fi

    if [ ! -f "$_expected" ]; then
        fail "$_name: missing golden $_expected (run with --update)"
    elif ! diff -u "$_expected" "$_out"; then
        fail "$_name: output differs from golden"
    fi

    if [ -s "$_err" ]; then
        printf 'note: %s emitted warnings:\n' "$_name"
        sed 's/^/  /' "$_err"
    fi
}

run_invalid_fixture() {
    _fixture=$1
    _name=$(basename "$_fixture" .uci)
    _out="$RUN_TMP/$_name.json"
    _err="$RUN_TMP/$_name.err"

    RUST_DNS_TEST_UCI_FILE=$_fixture
    export RUST_DNS_TEST_UCI_FILE

    if ( rust_dns_render ) > "$_out" 2> "$_err"; then
        fail "$_name: expected renderer failure"
        return
    fi
    [ -s "$_err" ] || fail "$_name: expected an error message on stderr"
    [ -s "$_out" ] && fail "$_name: renderer wrote partial output"
}

stub_smoke() {
    [ -x "$STUB" ] || { fail "stub is not executable"; return; }
    _cfg="$HERE/fixtures/json/basic.json"
    _data="$RUN_TMP/stub-data"

    "$STUB" version > "$RUN_TMP/version.out" 2>&1 || fail "stub version"
    grep -q '^rust-dns ' "$RUN_TMP/version.out" || fail "stub version output"

    "$STUB" check -c "$_cfg" || fail "stub check (json)"
    "$STUB" check -c "$PKG_DIR/files/config_custom.yaml" || fail "stub check (yaml custom)"
    RUST_DNS_STUB_ONCE=1 "$STUB" start -c "$_cfg" -d "$_data" || fail "stub start"

    # A deliberately broken config must be rejected.
    printf '{"api":{"http":"8.8.8.8:53"},"plugins":[]}' > "$RUN_TMP/bad.json"
    if "$STUB" check -c "$RUN_TMP/bad.json" 2>/dev/null; then
        fail "stub check accepted a non-loopback api"
    fi

    # A missing option argument must fail with a readable message, not a
    # shell "unbound variable" error (the stub runs under set -u).
    if "$STUB" check -c 2>"$RUN_TMP/stub-arg.err"; then
        fail "stub check accepted -c without a path"
    fi
    grep -qs 'requires a path' "$RUN_TMP/stub-arg.err" ||
        fail "stub check -c without a path did not explain the error"
}

# Optional: validate every golden fixture with the real daemon when the caller
# points RUST_DNS_BIN at a runnable binary (the release binary is per-arch).
real_binary_check() {
    _bin=${RUST_DNS_BIN:-}
    [ -n "$_bin" ] || return 0
    [ -x "$_bin" ] || { fail "RUST_DNS_BIN is not executable: $_bin"; return; }

    "$_bin" version >/dev/null 2>&1 || fail "real binary version failed"
    for _g in "$HERE"/fixtures/json/*.json; do
        [ -f "$_g" ] || continue
        "$_bin" check -c "$_g" >/dev/null 2>&1 ||
            fail "real binary rejected $(basename "$_g")"
    done
    "$_bin" check -c "$PKG_DIR/files/config_custom.yaml" >/dev/null 2>&1 ||
        fail "real binary rejected files/config_custom.yaml"
}

printf '== rust-dns config generator tests ==\n'
lint_scripts

for _fixture in "$HERE"/fixtures/invalid/*.uci; do
    [ -f "$_fixture" ] || continue
    run_invalid_fixture "$_fixture"
done

for _fixture in "$HERE"/fixtures/uci/*.uci; do
    [ -f "$_fixture" ] || continue
    run_valid_fixture "$_fixture"
done

stub_smoke
real_binary_check

if [ "$FAILURES" -gt 0 ]; then
    printf '\n%d failure(s)\n' "$FAILURES" >&2
    exit 1
fi
printf '\nall config generator tests passed\n'
