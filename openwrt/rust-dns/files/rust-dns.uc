#!/usr/bin/ucode
// SPDX-License-Identifier: GPL-3.0-only
//
// rust-dns geodata / adlist update tool.
//
//   ucode /usr/share/rust-dns/rust-dns.uc update [--all|--geosite|--geoip|--adlist]
//   ucode /usr/share/rust-dns/rust-dns.uc upgrade [--check]
//   ucode /usr/share/rust-dns/rust-dns.uc status
//   ucode /usr/share/rust-dns/rust-dns.uc clean
//
// Design contract (11-23-rust-dns-service/design.md section 9):
//   * atomic mkdir lock in /var/lock; a live owner means exit code 2;
//   * all downloads and conversions happen in /var/rust-dns/.staging.<pid>;
//   * existing files are only replaced after validation succeeds;
//   * one generation of backups is kept in /var/rust-dns/.bak;
//   * the previous data is never removed on failure;
//   * every update run appends "UPDATE_EXITED <code>" to
//     /var/rust-dns/update.log.
//
// Upgrade contract:
//   * --check writes /var/rust-dns/upgrade-check.json and prints the same JSON;
//   * upgrade stages packages in /var/rust-dns/.upgrade.<pid>, verifies every
//     file against SHA256SUMS-packages and appends "UPGRADE_EXITED <code>" to
//     /var/rust-dns/upgrade.log;
//   * only release assets for the running OpenWrt branch and arch are used.
//
// Exit codes: 0 ok, 2 lock held, 3 download failure, 4 validation failure,
//             5 conversion/install failure, 6 IO/permission/other.

'use strict';

import * as fs from 'fs';
import * as uci from 'uci';

const DATA_DIR = '/var/rust-dns';
const LOCK_DIR = '/var/lock/rust-dns_update.lock';
const UPGRADE_LOCK_DIR = '/var/lock/rust-dns_upgrade.lock';
const LOG_FILE = DATA_DIR + '/update.log';
const UPGRADE_LOG_FILE = DATA_DIR + '/upgrade.log';
const UPGRADE_CHECK_FILE = DATA_DIR + '/upgrade-check.json';
const MANIFEST = DATA_DIR + '/manifest.json';
const BACKUP_DIR = DATA_DIR + '/.bak';
const MAX_LOG = 262144;
const RELEASE_API = 'https://api.github.com/repos/moliango/rust-dns/releases/latest';
const SUMS_ASSET = 'SHA256SUMS-packages';

let lock_held = false;
let upgrade_lock_held = false;
let upgrade_marker_written = false;

// ---------------------------------------------------------------------------
// shell / file helpers
// ---------------------------------------------------------------------------

function shq(s) {
	// POSIX single-quote quoting without relying on regex replacement.
	let parts = split(`${s}`, "'");
	let out = parts[0];
	for (let i = 1; i < length(parts); i++)
		out += "'\\''" + parts[i];
	return "'" + out + "'";
}

function run(cmd) {
	try {
		return system(cmd);
	}
	catch (e) {
	}

	// Fallback for ucode builds without the system() builtin.  Output is
	// discarded; callers that need output redirect it to a file themselves.
	try {
		let p = fs.popen(cmd, 'r');
		if (p) {
			p.read('all');
			let rc = p.close();
			return rc == null ? 0 : rc;
		}
	}
	catch (e) {
	}

	return -1;
}

function mypid() {
	try {
		let link = fs.readlink('/proc/self');
		if (link != null && match(link, /^[0-9]+$/))
			return int(link);
	}
	catch (e) {
	}

	try {
		return getpid();
	}
	catch (e) {
	}

	return 0;
}

function file_exists(path) {
	return fs.stat(path) != null;
}

function file_size(path) {
	let st = fs.stat(path);
	return st ? st.size : 0;
}

function read_file(path) {
	try {
		return fs.readfile(path) ?? '';
	}
	catch (e) {
		return '';
	}
}

function write_file(path, data) {
	try {
		fs.writefile(path, data);
		return true;
	}
	catch (e) {
		return false;
	}
}

function write_atomic(path, data) {
	let tmp = path + '.tmp';
	if (!write_file(tmp, data))
		return false;
	try {
		fs.rename(tmp, path);
		return true;
	}
	catch (e) {
		return false;
	}
}

function try_mkdir(path) {
	// fs.mkdir() throwing means EEXIST (or another error); the caller decides
	// whether the directory is already-ours.  Returning the result is what
	// makes mkdir() usable as the atomic lock gate.
	try {
		fs.mkdir(path);
		return true;
	}
	catch (e) {
		return false;
	}
}

function lsdir_safe(path) {
	try {
		return fs.lsdir(path) ?? [];
	}
	catch (e) {
		return [];
	}
}

function is_true(v) {
	return v == 1 || v == '1' || v == true || v == 'true' || v == 'on' || v == 'yes';
}

function opt(name, def) {
	let ctx = null;
	try {
		ctx = uci.cursor();
	}
	catch (e) {
		return def;
	}
	let v = ctx.get('rust-dns', 'config', name);
	return v == null ? def : v;
}

function opt_list(name) {
	let v = opt(name, []);
	if (type(v) == 'array')
		return v;
	// Accept `option foo 'bar'` as a one-element list as well.
	if (type(v) == 'string' && trim(v) != '')
		return [ trim(v) ];
	return [];
}

function safe_tag(tag) {
	tag = trim(`${tag}`);
	return match(tag, /^[A-Za-z0-9_-]+$/) ? tag : null;
}

function line_count(path) {
	let data = read_file(path);
	if (data == '')
		return 0;
	let lines = split(data, '\n');
	let n = 0;
	for (let i = 0; i < length(lines); i++) {
		let line = trim(lines[i]);
		if (line != '' && substr(line, 0, 1) != '#')
			n++;
	}
	return n;
}

function timestamp() {
	let tmp = DATA_DIR + '/.timestamp';
	run('date -u +%Y-%m-%dT%H:%M:%SZ > ' + shq(tmp));
	let ts = trim(read_file(tmp));
	run('rm -f ' + shq(tmp));
	return ts != '' ? ts : 'unknown';
}

function append_log(path, msg) {
	run('mkdir -p ' + shq(DATA_DIR));
	if (file_size(path) > MAX_LOG)
		run('tail -c ' + (MAX_LOG / 2) + ' ' + shq(path) + ' > ' + shq(path + '.tmp') +
		    ' && mv -f ' + shq(path + '.tmp') + ' ' + shq(path));

	let line = sprintf('%s %s\n', timestamp(), msg);
	printf('%s', line);
	let handle = null;
	try {
		handle = fs.open(path, 'a');
	}
	catch (e) {
		return;
	}
	if (handle) {
		handle.write(line);
		handle.close();
	}
}

function log(msg) {
	append_log(LOG_FILE, msg);
}

function upgrade_log(msg) {
	append_log(UPGRADE_LOG_FILE, msg);
}

function sha256(path) {
	let out = DATA_DIR + '/.sha256';
	run('sha256sum ' + shq(path) + ' > ' + shq(out) + ' 2>/dev/null');
	let line = trim(read_file(out));
	run('rm -f ' + shq(out));
	if (line == '')
		return null;
	return split(line, ' ')[0];
}

// ---------------------------------------------------------------------------
// lock
// ---------------------------------------------------------------------------

function lock_pid() {
	let pid = trim(read_file(LOCK_DIR + '/pid'));
	return match(pid, /^[0-9]+$/) ? pid : null;
}

// An existing lock directory without a pid file is either mid-acquisition or
// abandoned.  Only an old directory may be reclaimed; otherwise we treat it as
// held so two concurrent runs cannot both "win" the lock.
function lock_stale() {
	let st = null;
	try {
		st = fs.stat(LOCK_DIR);
	}
	catch (e) {
		return false;
	}
	if (st == null || st.mtime == null)
		return false;
	let now = null;
	try {
		now = time();
	}
	catch (e) {
		return false;
	}
	return (now - st.mtime) > 120;
}

function acquire_lock() {
	run('mkdir -p ' + shq('/var/lock'));

	let me = sprintf('%d', mypid());

	// mkdir() is the atomic gate: only the process that created the directory
	// may claim it.
	if (try_mkdir(LOCK_DIR)) {
		write_file(LOCK_DIR + '/pid', me);
		lock_held = true;
		return true;
	}

	let pid = lock_pid();
	if (pid == me) {
		// Re-entrant acquisition by the same process.
		lock_held = true;
		return true;
	}
	if (pid != null && file_exists('/proc/' + pid))
		return false;               // live owner

	if (pid == null && !lock_stale())
		return false;               // another process is still acquiring it

	// Dead pid (or an abandoned directory): reclaim it exactly once.  Losing
	// the mkdir race here means someone else reclaimed it first.
	run('rm -rf ' + shq(LOCK_DIR));
	if (!try_mkdir(LOCK_DIR))
		return false;
	write_file(LOCK_DIR + '/pid', me);
	lock_held = true;
	return true;
}

function release_lock() {
	if (!lock_held)
		return;
	let me = sprintf('%d', mypid());
	let pid = lock_pid();
	if (pid == null || pid == me)
		run('rm -rf ' + shq(LOCK_DIR));
	lock_held = false;
}

// The upgrade lock is independent of the update lock: refreshable data and a
// package upgrade may be started from different pages and should not block
// each other.  Acquisition/retirement semantics mirror the update lock.
function upgrade_lock_pid() {
	let pid = trim(read_file(UPGRADE_LOCK_DIR + '/pid'));
	return match(pid, /^[0-9]+$/) ? pid : null;
}

function upgrade_lock_stale() {
	let st = null;
	try {
		st = fs.stat(UPGRADE_LOCK_DIR);
	}
	catch (e) {
		return false;
	}
	if (st == null || st.mtime == null)
		return false;
	let now = null;
	try {
		now = time();
	}
	catch (e) {
		return false;
	}
	return (now - st.mtime) > 120;
}

function acquire_upgrade_lock() {
	run('mkdir -p ' + shq('/var/lock'));

	let me = sprintf('%d', mypid());

	if (try_mkdir(UPGRADE_LOCK_DIR)) {
		write_file(UPGRADE_LOCK_DIR + '/pid', me);
		upgrade_lock_held = true;
		return true;
	}

	let pid = upgrade_lock_pid();
	if (pid == me) {
		upgrade_lock_held = true;
		return true;
	}
	if (pid != null && file_exists('/proc/' + pid))
		return false;

	if (pid == null && !upgrade_lock_stale())
		return false;

	run('rm -rf ' + shq(UPGRADE_LOCK_DIR));
	if (!try_mkdir(UPGRADE_LOCK_DIR))
		return false;
	write_file(UPGRADE_LOCK_DIR + '/pid', me);
	upgrade_lock_held = true;
	return true;
}

function release_upgrade_lock() {
	if (!upgrade_lock_held)
		return;
	let me = sprintf('%d', mypid());
	let pid = upgrade_lock_pid();
	if (pid == null || pid == me)
		run('rm -rf ' + shq(UPGRADE_LOCK_DIR));
	upgrade_lock_held = false;
}

// ---------------------------------------------------------------------------
// download / conversion
// ---------------------------------------------------------------------------

function download(url, dest) {
	// Local fixtures and offline mirrors use file:// URLs; uclient-fetch does
	// not speak that scheme.
	if (substr(url, 0, 7) == 'file://') {
		let src = substr(url, 7);
		run('rm -f ' + shq(dest));
		run('cp -f ' + shq(src) + ' ' + shq(dest) + ' 2>/dev/null');
		return file_size(dest) > 0;
	}

	let full = url;
	let proxy = opt('github_proxy', '');
	if (proxy != '' && index(url, 'github.com') >= 0)
		full = proxy + url;

	run('rm -f ' + shq(dest));
	let rc = run('uclient-fetch -q -T 30 -O ' + shq(dest) + ' ' + shq(full) + ' 2>/dev/null');
	return rc == 0 && file_size(dest) > 0;
}

// Unpack one tag with geo2txt (the tool shipped by the geo2txt package):
//   geo2txt geosite -f geosite.dat -e cn -o <outdir>
// It writes <outdir>/geosite_<tag>.txt (or geoip_<tag>.txt).
function dump_tag(kind, datfile, tag, outdir) {
	let outfile = sprintf('%s/%s_%s.txt', outdir, kind, tag);

	run('rm -f ' + shq(outfile));
	let rc = run('geo2txt ' + shq(kind) + ' -f ' + shq(datfile) +
		' -e ' + shq(tag) + ' -o ' + shq(outdir) + ' 2>/dev/null');

	return rc == 0 && file_size(outfile) > 0;
}

function installed_dat(kind) {
	let path = (kind == 'geosite') ? '/usr/share/v2ray/geosite.dat'
	                              : '/usr/share/v2ray/geoip.dat';
	return file_exists(path) ? path : null;
}

function download_dat(kind, staging) {
	let url = (kind == 'geosite')
		? 'https://github.com/v2fly/domain-list-community/releases/latest/download/dlc.dat'
		: 'https://github.com/v2fly/geoip/releases/latest/download/geoip.dat';
	let dest = staging + '/' + kind + '.dat';
	return download(url, dest) ? dest : null;
}

function build_geodata(staging, kind) {
	// Prefer the installed dat.  Download a fresh one when there is no local
	// dat at all (manual update after only installing geo2txt) or when the
	// user asked for automatic updates; keep the installed dat if the
	// download fails.
	let dat = installed_dat(kind);

	if (dat == null || is_true(opt('geo_auto_update', 0))) {
		let downloaded = download_dat(kind, staging);
		if (downloaded != null)
			dat = downloaded;
	}

	if (dat == null) {
		log(kind + ': no .dat source available');
		return false;
	}

	if (run('command -v geo2txt >/dev/null 2>&1') != 0) {
		log(kind + ': geo2txt not found; install the geo2txt package');
		return false;
	}

	let tags = opt_list(kind == 'geosite' ? 'geosite_tags' : 'geoip_tags');
	if (length(tags) == 0)
		push(tags, 'cn');

	let produced = false;
	for (let i = 0; i < length(tags); i++) {
		let tag = safe_tag(tags[i]);
		if (tag == null)
			continue;
		if (dump_tag(kind, dat, tag, staging))
			produced = true;
		else
			log(sprintf('%s: geo2txt failed for tag %s', kind, tag));
	}

	// The generated config references the fixed *_cn.txt paths; make sure they
	// exist even when the user only selected custom tags.
	let cn = sprintf('%s/%s_cn.txt', staging, kind);
	if (!file_exists(cn) && dump_tag(kind, dat, 'cn', staging))
		produced = true;

	return produced;
}

function build_adlist(staging) {
	let urls = opt_list('ad_source');
	if (length(urls) == 0) {
		log('adlist: no ad_source configured; keeping existing data');
		return true;
	}

	let seen = {};
	let domains = [];
	let idx = 0;

	for (let u = 0; u < length(urls); u++) {
		if (type(urls[u]) != 'string')
			continue;
		let url = trim(urls[u]);
		if (url == '')
			continue;

		idx++;
		let tmp = sprintf('%s/adlist.%d.dl', staging, idx);
		if (!download(url, tmp)) {
			log('adlist: download failed: ' + url);
			return false;
		}

		let data = read_file(tmp);
		run('rm -f ' + shq(tmp));

		let lines = split(data, '\n');
		for (let l = 0; l < length(lines); l++) {
			let d = trim(split(lines[l], '#')[0]);
			d = trim(split(d, ' ')[0]);
			if (d == '')
				continue;
			if (!match(d, /^[A-Za-z0-9_.-]+$/))
				continue;
			if (seen[d])
				continue;
			seen[d] = true;
			push(domains, d);
		}
	}

	if (length(domains) == 0) {
		log('adlist: no valid domains produced');
		return false;
	}

	let out = staging + '/adlist.txt';
	return write_file(out, join('\n', domains) + '\n');
}

function validate_staged(staging) {
	let names = lsdir_safe(staging);
	let found = 0;

	for (let i = 0; i < length(names); i++) {
		let name = names[i];
		if (length(name) <= 4 || substr(name, length(name) - 4) != '.txt')
			continue;
		let path = staging + '/' + name;
		if (file_size(path) <= 0 || trim(read_file(path)) == '') {
			log('validation failed for ' + name);
			return false;
		}
		found++;
	}

	if (found == 0) {
		log('validation failed: no output files');
		return false;
	}
	return true;
}

function file_entry(path) {
	return {
		sha256: sha256(path),
		count: line_count(path),
		bytes: file_size(path)
	};
}

// manifest_group <entries> <preferred-name>
// The design contract is { sha256, count } per source.  Keep that summary for
// the primary file (the one the generated config references) and expose every
// produced file under .files.
function manifest_group(entries, preferred) {
	let files = {};
	let primary = null;

	for (let i = 0; i < length(entries); i++) {
		let name = entries[i][0];
		let entry = entries[i][1];
		files[name] = entry;
		if (name == preferred)
			primary = entry;
	}

	if (primary == null && length(entries) > 0)
		primary = entries[0][1];

	return {
		sha256: primary ? primary.sha256 : null,
		count: primary ? primary.count : 0,
		bytes: primary ? primary.bytes : 0,
		files: files
	};
}

function manifest_for(staging) {
	let manifest = { generated_at: timestamp() };
	let geosite = [];
	let geoip = [];
	let adlist = [];
	let names = lsdir_safe(staging);

	for (let i = 0; i < length(names); i++) {
		let name = names[i];
		if (length(name) <= 4 || substr(name, length(name) - 4) != '.txt')
			continue;
		let entry = file_entry(staging + '/' + name);
		if (substr(name, 0, 7) == 'geosite_')
			push(geosite, [name, entry]);
		else if (substr(name, 0, 6) == 'geoip_')
			push(geoip, [name, entry]);
		else if (name == 'adlist.txt')
			push(adlist, [name, entry]);
	}

	manifest.geosite = manifest_group(geosite, 'geosite_cn.txt');
	manifest.geoip = manifest_group(geoip, 'geoip_cn.txt');
	manifest.adlist = manifest_group(adlist, 'adlist.txt');

	return manifest;
}

function commit_file(staged, dest) {
	let name = split(dest, '/');
	name = name[length(name) - 1];

	run('mkdir -p ' + shq(BACKUP_DIR));
	if (file_exists(dest)) {
		// The backup is the only rollback source, so a failed copy must stop
		// the commit instead of overwriting the previous generation.
		let rc = run('cp -f ' + shq(dest) + ' ' + shq(BACKUP_DIR + '/' + name + '.bak') + ' 2>/dev/null');
		if (rc != 0) {
			log(sprintf('cannot back up %s before commit; aborting', name));
			return false;
		}
	}

	try {
		fs.rename(staged, dest);
	}
	catch (e) {
		log(sprintf('commit failed for %s: %s', name, e));
		return false;
	}
	return file_exists(dest);
}

// ---------------------------------------------------------------------------
// upgrade: detection, version and asset selection helpers
// ---------------------------------------------------------------------------

function capture(cmd) {
	run('mkdir -p ' + shq(DATA_DIR));
	let tmp = sprintf('%s/.capture.%d', DATA_DIR, mypid());
	run('rm -f ' + shq(tmp));
	let rc = run(cmd + ' > ' + shq(tmp) + ' 2>/dev/null');
	let out = read_file(tmp);
	run('rm -f ' + shq(tmp));
	return { code: rc, output: out };
}

// Accept 0.1.0, 0.1.0-r4 (24.10) and 0.1.0-4 (23.05), with an optional
// leading v.  Only major.minor.patch is compared; the revision is returned
// separately so duplicate builds can be ordered.
function parse_version(v) {
	let m = match(trim(`${v}`), /^v?([0-9]+(\.[0-9]+)*)(-r?([0-9]+))?$/);
	if (m == null)
		return null;
	return {
		version: m[1],
		revision: m[4] != null ? int(m[4]) : 0
	};
}

function strip_version(v) {
	let p = parse_version(v);
	if (p != null)
		return p.version;

	v = trim(`${v}`);
	if (substr(v, 0, 1) == 'v')
		v = substr(v, 1);
	let m = match(v, /^(.*)-r?[0-9]+$/);
	return m != null ? m[1] : v;
}

function version_parts(v) {
	let parts = split(trim(`${v}`), '.');
	let out = [];
	for (let i = 0; i < length(parts); i++) {
		let m = match(parts[i], /^[0-9]+/);
		push(out, m != null ? int(m[0]) : 0);
	}
	return out;
}

function version_cmp(a, b) {
	let x = version_parts(a);
	let y = version_parts(b);
	let n = length(x) > length(y) ? length(x) : length(y);
	for (let i = 0; i < n; i++) {
		let xi = i < length(x) ? x[i] : 0;
		let yi = i < length(y) ? y[i] : 0;
		if (xi > yi)
			return 1;
		if (xi < yi)
			return -1;
	}
	return 0;
}

function version_gt(a, b) {
	if (a == null || a == '')
		return false;
	if (b == null || b == '')
		return true;
	return version_cmp(a, b) > 0;
}

function parse_release_field(data, key) {
	let prefix = key + '=';
	let lines = split(data, '\n');
	for (let i = 0; i < length(lines); i++) {
		let line = trim(lines[i]);
		if (substr(line, 0, length(prefix)) != prefix)
			continue;
		let v = trim(substr(line, length(prefix)));
		let n = length(v);
		if (n >= 2 && ((substr(v, 0, 1) == "'" && substr(v, n - 1, 1) == "'") ||
		               (substr(v, 0, 1) == '"' && substr(v, n - 1, 1) == '"')))
			v = substr(v, 1, n - 2);
		return trim(v);
	}
	return '';
}

function branch_from(release) {
	let m = match(trim(`${release}`), /^([0-9]+)\.([0-9]+)/);
	return m != null ? m[1] + '.' + m[2] : '';
}

function package_manager() {
	if (file_exists('/usr/bin/opkg'))
		return 'opkg';
	if (run('command -v opkg >/dev/null 2>&1') == 0)
		return 'opkg';
	return 'apk';
}

function version_from_opkg(text) {
	let lines = split(text, '\n');
	for (let i = 0; i < length(lines); i++) {
		let m = match(trim(lines[i]), /^Version:[ \t]*(.+)$/);
		if (m != null)
			return strip_version(m[1]);
	}
	return '';
}

function version_from_apk(text) {
	let lines = split(text, '\n');
	for (let i = 0; i < length(lines); i++) {
		let m = match(trim(lines[i]), /^rust-dns-([^ \t]+)/);
		if (m != null)
			return strip_version(m[1]);
	}
	return '';
}

function installed_version() {
	let r = capture('opkg status rust-dns');
	let v = version_from_opkg(r.output);
	if (v != '')
		return v;
	r = capture('apk info -v rust-dns');
	return version_from_apk(r.output);
}

function fallback_arch() {
	let r = capture('opkg print-architecture');
	let lines = split(r.output, '\n');
	let first = '';
	for (let i = 0; i < length(lines); i++) {
		let m = match(trim(lines[i]), /^arch[ \t]+([^ \t]+)/);
		if (m == null)
			continue;
		if (first == '')
			first = m[1];
		if (m[1] != 'all' && m[1] != 'noarch')
			return m[1];
	}
	return first;
}

function detect_context() {
	let pm = package_manager();
	let rel = read_file('/etc/openwrt_release');
	let release = parse_release_field(rel, 'DISTRIB_RELEASE');
	let arch = parse_release_field(rel, 'DISTRIB_ARCH');
	if (arch == '')
		arch = fallback_arch();

	return {
		current: installed_version(),
		arch: arch,
		openwrt: release,
		branch: branch_from(release),
		pm: pm,
		ext: pm == 'opkg' ? '.ipk' : '.apk'
	};
}

function has_suffix(s, suffix) {
	return length(s) >= length(suffix) &&
	       substr(s, length(s) - length(suffix)) == suffix;
}

// Token that identifies a package inside a release asset filename.  ipk names
// use underscores around the package name; apk names use `<pkg>-<version>`.
function asset_token(category, ext) {
	if (category == 'i18n')
		return '_luci-i18n-rust-dns-';
	return '_' + category + (ext == '.ipk' ? '_' : '-');
}

// ipk: <label>_<archlabel>_<pkg>_<version>_<pkgarch>.ipk
// apk: <label>_<archlabel>_<pkg>-<version>.apk
// Versions may be 0.1.1, 0.1.1-r1 or 0.1.1-1 depending on the OpenWrt branch.
function asset_info(name, category, ext) {
	let token = asset_token(category, ext);
	let idx = index(name, token);
	if (idx < 0)
		return null;

	let rest = substr(name, idx + length(token));
	let raw = null;
	let lang = null;

	if (category == 'i18n') {
		if (ext == '.ipk') {
			let parts = split(rest, '_');
			if (length(parts) < 2)
				return null;
			lang = parts[0];
			raw = parts[1];
		}
		else {
			if (!has_suffix(rest, '.apk'))
				return null;
			let base = substr(rest, 0, length(rest) - 4);
			let m = match(base, /^(.*)-([0-9]+(\.[0-9]+)*(-r?[0-9]+)?)$/);
			if (m == null)
				return null;
			lang = m[1];
			raw = m[2];
		}
	}
	else if (ext == '.ipk') {
		raw = split(rest, '_')[0];
	}
	else {
		if (!has_suffix(rest, '.apk'))
			return null;
		raw = substr(rest, 0, length(rest) - 4);
	}

	let v = parse_version(raw);
	if (v == null)
		return null;

	return {
		file: name,
		category: category,
		lang: lang,
		base: v.version,
		revision: v.revision
	};
}

function asset_candidate(asset, ctx, category) {
	let name = asset.name;
	if (type(name) != 'string' || !has_suffix(name, ctx.ext))
		return null;
	// A 24.10 router only ever sees 24.10.* assets.
	if (ctx.branch == '' || substr(name, 0, length(ctx.branch) + 1) != ctx.branch + '.')
		return null;
	// Architecture token, e.g. _x86_64- or _arm_cortex-a7_neon-vfpv4-.
	if (index(name, '_' + ctx.arch + '-') < 0)
		return null;
	if (index(name, asset_token(category, ctx.ext)) < 0)
		return null;
	return asset_info(name, category, ctx.ext);
}

// Pick the candidate matching the release tag's major.minor.patch, preferring
// the highest revision when an old release contains several rebuilds.
function select_best(candidates, expected) {
	let best = null;
	for (let i = 0; i < length(candidates); i++) {
		let c = candidates[i];
		if (expected != null && expected != '' && c.base != expected)
			continue;
		if (best == null) {
			best = c;
			continue;
		}
		let cmp = version_cmp(c.base, best.base);
		if (cmp > 0 || (cmp == 0 && c.revision > best.revision))
			best = c;
	}
	return best;
}

// At most one translation package is installed.  Prefer zh-cn, otherwise the
// first language GitHub reported, and the highest revision within it.
function select_i18n(candidates, expected) {
	let groups = {};
	let order = [];
	for (let i = 0; i < length(candidates); i++) {
		let c = candidates[i];
		if (expected != null && expected != '' && c.base != expected)
			continue;
		if (groups[c.lang] == null) {
			groups[c.lang] = [];
			push(order, c.lang);
		}
		push(groups[c.lang], c);
	}
	if (length(order) == 0)
		return null;
	let lang = groups['zh-cn'] != null ? 'zh-cn' : order[0];
	return select_best(groups[lang], expected);
}

function select_packages(release, ctx, latest) {
	let assets = release.assets;
	if (type(assets) != 'array')
		return { code: 4, error: 'release contains no package assets', packages: [], sums_url: null };

	let sums_url = null;
	let groups = { geo2txt: [], 'rust-dns': [], 'luci-app-rust-dns': [], i18n: [] };
	let names = [ 'geo2txt', 'rust-dns', 'luci-app-rust-dns', 'i18n' ];

	for (let i = 0; i < length(assets); i++) {
		let a = assets[i];
		if (type(a) != 'object' || type(a.name) != 'string')
			continue;
		if (a.name == SUMS_ASSET) {
			if (sums_url == null && type(a.browser_download_url) == 'string')
				sums_url = a.browser_download_url;
			continue;
		}
		for (let g = 0; g < length(names); g++) {
			let c = asset_candidate(a, ctx, names[g]);
			if (c != null) {
				if (type(a.browser_download_url) == 'string' && a.browser_download_url != '')
					c.url = a.browser_download_url;
				else
					c.url = null;
				push(groups[names[g]], c);
				break;
			}
		}
	}

	if (sums_url == null)
		return { code: 4, error: 'release does not provide ' + SUMS_ASSET, packages: [], sums_url: null };

	// Install order matters: geo2txt is the dependency of rust-dns, and the
	// LuCI packages are installed last.
	let selected = [];

	let geo = select_best(groups.geo2txt, null);
	if (geo != null)
		push(selected, geo);

	let svc = select_best(groups['rust-dns'], latest);
	if (svc == null)
		return {
			code: 4,
			error: sprintf('no matching rust-dns package for branch %s arch %s',
			               ctx.branch != '' ? ctx.branch : 'unknown',
			               ctx.arch != '' ? ctx.arch : 'unknown'),
			packages: [],
			sums_url: sums_url
		};
	push(selected, svc);

	let luci = select_best(groups['luci-app-rust-dns'], latest);
	if (luci != null)
		push(selected, luci);

	// LuCI translation packages use their own date-based version, so they are
	// not filtered against the release tag; just prefer zh-cn and the highest
	// revision among the branch/arch-compatible candidates.
	let i18n = select_i18n(groups.i18n, null);
	if (i18n != null)
		push(selected, i18n);

	for (let i = 0; i < length(selected); i++) {
		if (selected[i].url == null)
			return { code: 4, error: 'no download URL for ' + selected[i].file, packages: [], sums_url: sums_url };
	}

	return { code: 0, error: null, packages: selected, sums_url: sums_url };
}

function parse_checksums(text) {
	let map = {};
	let lines = split(text, '\n');
	for (let i = 0; i < length(lines); i++) {
		let line = trim(lines[i]);
		if (line == '')
			continue;
		let m = match(line, /^([0-9a-fA-F]{64})[ \t]+\*?(.+)$/);
		if (m == null)
			continue;
		map[trim(m[2])] = lc(m[1]);
	}
	return map;
}

function empty_check_result(ctx) {
	return {
		current: ctx != null ? ctx.current : '',
		latest: null,
		tag: null,
		update_available: false,
		arch: ctx != null ? ctx.arch : '',
		openwrt: ctx != null ? ctx.openwrt : '',
		branch: ctx != null ? ctx.branch : '',
		package_manager: ctx != null ? ctx.pm : '',
		packages: [],
		error: null
	};
}

function check_fail(result, msg, code) {
	result.error = msg;
	return { code: code, result: result };
}

// Query GitHub and select the matching packages + checksums.  This never
// writes the check file; the caller decides whether to publish the result.
function build_check() {
	let ctx = detect_context();
	let result = empty_check_result(ctx);
	let tmpdir = sprintf('%s/.upgrade-check.%d', DATA_DIR, mypid());
	let api_file = tmpdir + '/release.json';
	let sums_file = tmpdir + '/SHA256SUMS-packages';

	run('rm -rf ' + shq(tmpdir));
	run('mkdir -p ' + shq(tmpdir));

	if (!download(RELEASE_API, api_file)) {
		run('rm -rf ' + shq(tmpdir));
		return check_fail(result, 'failed to fetch release metadata from GitHub', 3);
	}

	let release = null;
	try {
		release = json(read_file(api_file));
	}
	catch (e) {
		release = null;
	}
	if (type(release) != 'object' || release.tag_name == null || `${release.tag_name}` == '') {
		run('rm -rf ' + shq(tmpdir));
		return check_fail(result, 'invalid release metadata from GitHub', 3);
	}

	result.tag = `${release.tag_name}`;
	result.latest = strip_version(result.tag);
	result.update_available = result.latest != '' &&
		(result.current == '' || version_gt(result.latest, result.current));

	let selected = select_packages(release, ctx, result.latest);
	if (selected.code != 0) {
		run('rm -rf ' + shq(tmpdir));
		return check_fail(result, selected.error, selected.code);
	}

	if (!download(selected.sums_url, sums_file)) {
		run('rm -rf ' + shq(tmpdir));
		return check_fail(result, 'failed to download ' + SUMS_ASSET, 3);
	}

	let sums = parse_checksums(read_file(sums_file));
	let packages = [];
	for (let i = 0; i < length(selected.packages); i++) {
		let p = selected.packages[i];

		// Asset names become local staging paths.  GitHub releases normally
		// contain plain filenames, but never let a compromised/malformed name
		// escape the staging directory or reach the shell.
		if (match(p.file, /^[A-Za-z0-9._+-]+$/) == null || index(p.file, '..') >= 0) {
			run('rm -rf ' + shq(tmpdir));
			return check_fail(result, 'unsafe release asset name: ' + p.file, 4);
		}
		if (substr(p.url, 0, 8) != 'https://') {
			run('rm -rf ' + shq(tmpdir));
			return check_fail(result, 'asset URL is not HTTPS: ' + p.file, 4);
		}

		let hash = sums[p.file];
		if (hash == null || hash == '') {
			run('rm -rf ' + shq(tmpdir));
			return check_fail(result, 'missing checksum for ' + p.file, 4);
		}
		push(packages, {
			name: p.category == 'i18n' ? 'luci-i18n-rust-dns-' + p.lang : p.category,
			file: p.file,
			url: p.url,
			sha256: hash
		});
	}

	result.packages = packages;
	run('rm -rf ' + shq(tmpdir));
	return { code: 0, result: result };
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

function cmd_update(kinds) {
	let wanted = { geosite: false, geoip: false, adlist: false };
	if (length(kinds) == 0) {
		wanted.geosite = true;
		wanted.geoip = true;
		wanted.adlist = true;
	}
	else {
		for (let i = 0; i < length(kinds); i++) {
			let k = kinds[i];
			if (k == 'geosite' || k == 'geoip' || k == 'adlist')
				wanted[k] = true;
		}
	}

	if (!acquire_lock()) {
		log('update already running; lock held by ' +
		    (lock_pid() ?? 'unknown') + ' (try again later)');
		return 2;
	}

	let staging = sprintf('%s/.staging.%d', DATA_DIR, mypid());
	run('rm -rf ' + shq(staging));
	run('mkdir -p ' + shq(staging));

	let code = 0;

	if (wanted.geosite && !build_geodata(staging, 'geosite'))
		code = 5;
	if (code == 0 && wanted.geoip && !build_geodata(staging, 'geoip'))
		code = 5;
	if (code == 0 && wanted.adlist && !build_adlist(staging))
		code = 3;

	// Drop downloaded .dat files; they are only conversion inputs.
	let staged = lsdir_safe(staging);
	for (let i = 0; i < length(staged); i++) {
		let name = staged[i];
		if (length(name) > 4 && substr(name, length(name) - 4) == '.dat')
			run('rm -f ' + shq(staging + '/' + name));
	}

	let expected = wanted.geosite || wanted.geoip ||
		(wanted.adlist && length(opt_list('ad_source')) > 0);

	if (code == 0 && !expected) {
		log('nothing to do: no geodata sources and no ad_source configured');
		run('rm -rf ' + shq(staging));
		release_lock();
		return 0;
	}

	if (code == 0 && !validate_staged(staging))
		code = 4;

	if (code == 0) {
		let manifest = manifest_for(staging);
		let names = lsdir_safe(staging);
		let committed = [];
		for (let i = 0; i < length(names); i++) {
			let name = names[i];
			if (length(name) <= 4 || substr(name, length(name) - 4) != '.txt')
				continue;
			let dest = DATA_DIR + '/' + name;
			if (!commit_file(staging + '/' + name, dest)) {
				code = 6;
				break;
			}
			push(committed, dest);
		}
		if (code == 0) {
			if (!write_atomic(MANIFEST, sprintf('%J\n', manifest)))
				code = 6;
		}
		if (code != 0) {
			// Never leave a mix of old and new generation files behind: put
			// back every file that was already replaced (a failed manifest
			// write must roll the data back too).
			for (let i = length(committed) - 1; i >= 0; i--) {
				let dest = committed[i];
				let base = split(dest, '/');
				base = base[length(base) - 1];
				let bak = BACKUP_DIR + '/' + base + '.bak';
				if (file_exists(bak))
					run('cp -f ' + shq(bak) + ' ' + shq(dest) + ' 2>/dev/null');
				else
					run('rm -f ' + shq(dest));
			}
		}
		if (code == 0) {
			log('update committed');
			run('rm -rf ' + shq(staging));
			run('/etc/init.d/rust-dns reload >/dev/null 2>&1');
		}
	}

	if (code != 0) {
		log(sprintf('update failed with code %d; existing data kept', code));
		run('rm -rf ' + shq(staging));
	}

	release_lock();
	return code;
}

function cmd_status() {
	if (!file_exists(MANIFEST)) {
		printf('no update data\n');
		return 1;
	}
	printf('%s', read_file(MANIFEST));
	return 0;
}

function cmd_clean() {
	let names = lsdir_safe(DATA_DIR);
	for (let i = 0; i < length(names); i++) {
		if (substr(names[i], 0, 9) == '.staging.')
			run('rm -rf ' + shq(DATA_DIR + '/' + names[i]));
	}
	run('rm -rf ' + shq(BACKUP_DIR));
	run('rm -f ' + shq(MANIFEST));
	log('cleaned staging data, backups and manifest');
	return 0;
}

function cmd_upgrade_check() {
	run('mkdir -p ' + shq(DATA_DIR));

	let built = null;
	try {
		built = build_check();
	}
	catch (e) {
		let result = empty_check_result(null);
		result.error = 'upgrade check failed: ' + e;
		built = { code: 6, result: result };
	}

	// The UI reads this file as well as stdout; write valid JSON even when
	// the check itself failed.
	let text = sprintf('%J\n', built.result);
	if (!write_atomic(UPGRADE_CHECK_FILE, text)) {
		built.result.error = built.result.error ?? ('failed to write ' + UPGRADE_CHECK_FILE);
		text = sprintf('%J\n', built.result);
		write_atomic(UPGRADE_CHECK_FILE, text);
		printf('%s', text);
		return 6;
	}

	printf('%s', text);
	return built.code;
}

function cmd_upgrade() {
	run('mkdir -p ' + shq(DATA_DIR));
	upgrade_log('upgrade: checking latest release');

	if (!acquire_upgrade_lock()) {
		upgrade_log('upgrade already running; lock held by ' +
		            (upgrade_lock_pid() ?? 'unknown') + ' (try again later)');
		return 2;
	}

	let code = 0;
	let staging = sprintf('%s/.upgrade.%d', DATA_DIR, mypid());

	try {
		let built = build_check();
		let result = built.result;

		if (built.code != 0) {
			upgrade_log('upgrade check failed: ' + (result.error ?? 'unknown error'));
			code = built.code;
		}
		else if (!result.update_available) {
			upgrade_log(sprintf('already up to date (%s)',
				result.current != '' ? result.current : 'unknown'));
			code = 0;
		}
		else {
			upgrade_log(sprintf('upgrading %s -> %s (%d package(s))',
				result.current != '' ? result.current : 'not installed',
				result.latest, length(result.packages)));

			run('rm -rf ' + shq(staging));
			run('mkdir -p ' + shq(staging));

			for (let i = 0; i < length(result.packages); i++) {
				let p = result.packages[i];
				let dest = staging + '/' + p.file;
				upgrade_log('downloading ' + p.file);
				if (!download(p.url, dest)) {
					upgrade_log('download failed: ' + p.file);
					code = 3;
					break;
				}
				let actual = sha256(dest);
				if (actual == null || lc(actual) != lc(p.sha256)) {
					upgrade_log('checksum mismatch: ' + p.file);
					code = 4;
					break;
				}
			}

			if (code == 0) {
				let quoted = [];
				for (let i = 0; i < length(result.packages); i++)
					push(quoted, shq(staging + '/' + result.packages[i].file));

				let verb = result.package_manager == 'opkg'
					? 'opkg install --force-reinstall '
					: 'apk add --allow-untrusted ';
				let outfile = staging + '/install.log';

				upgrade_log('installing with ' + result.package_manager);
				let rc = run(verb + join(' ', quoted) + ' > ' + shq(outfile) + ' 2>&1');
				if (rc != 0) {
					upgrade_log(sprintf('%s install failed (code %d)',
						result.package_manager, rc));
					let lines = split(read_file(outfile), '\n');
					for (let i = 0; i < length(lines); i++) {
						let line = trim(lines[i]);
						if (line != '')
							upgrade_log('  ' + line);
					}
					code = 5;
				}
			}

			if (code == 0) {
				upgrade_log('upgrade to ' + result.latest + ' complete');

				// Terminal marker before restarting rpcd/uhttpd: those
				// restarts can kill this detached shell, and the UI must
				// still see a final state.  The bottom of the script only
				// appends the marker when this path did not already do so.
				upgrade_log(sprintf('UPGRADE_EXITED %d', code));
				upgrade_marker_written = true;

				let rc = run('[ -x /etc/init.d/rust-dns ] && /etc/init.d/rust-dns enabled && /etc/init.d/rust-dns restart');
				if (rc != 0)
					upgrade_log(sprintf('rust-dns restart skipped or failed (code %d)', rc));

				rc = run('/etc/init.d/rpcd restart');
				if (rc != 0)
					upgrade_log(sprintf('rpcd restart failed (code %d)', rc));

				rc = run('/etc/init.d/uhttpd restart');
				if (rc != 0)
					upgrade_log(sprintf('uhttpd restart failed (code %d)', rc));
			}
		}
	}
	catch (e) {
		upgrade_log('upgrade error: ' + e);
		code = 6;
	}

	run('rm -rf ' + shq(staging));
	release_upgrade_lock();

	if (code != 0)
		upgrade_log(sprintf('upgrade failed with code %d', code));

	return code;
}

function usage() {
	printf('Usage: rust-dns.uc update [--all|--geosite|--geoip|--adlist]\n');
	printf('       rust-dns.uc upgrade [--check]\n');
	printf('       rust-dns.uc status\n');
	printf('       rust-dns.uc clean\n');
}

function upgrade_usage() {
	printf('Usage: rust-dns.uc upgrade [--check]\n');
	printf('       rust-dns.uc upgrade --help\n');
	printf('\n');
	printf('  --check   report the latest GitHub release as JSON without installing\n');
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

let exit_code = 0;
let run_mode = 'other';

try {
	let args = ARGV ?? [];
	let cmd = args[0] ?? 'status';

	if (cmd == 'update') {
		run_mode = 'update';
		let kinds = [];
		for (let i = 1; i < length(args); i++) {
			if (args[i] == '--all') {
				push(kinds, 'geosite');
				push(kinds, 'geoip');
				push(kinds, 'adlist');
			}
			else if (args[i] == '--geosite' || args[i] == '--geoip' || args[i] == '--adlist') {
				push(kinds, substr(args[i], 2));
			}
			else {
				usage();
				exit_code = 1;
				break;
			}
		}
		if (exit_code == 0)
			exit_code = cmd_update(kinds);
	}
	else if (cmd == 'upgrade') {
		let sub = args[1] ?? '';
		if (sub == '--check') {
			run_mode = 'upgrade-check';
			if (length(args) > 2) {
				upgrade_usage();
				exit_code = 1;
			}
			else {
				exit_code = cmd_upgrade_check();
			}
		}
		else if (sub == '--help' || sub == '-h') {
			upgrade_usage();
			exit_code = 0;
		}
		else if (sub == '') {
			run_mode = 'upgrade';
			exit_code = cmd_upgrade();
		}
		else {
			upgrade_usage();
			exit_code = 1;
		}
	}
	else if (cmd == 'status') {
		exit_code = cmd_status();
	}
	else if (cmd == 'clean') {
		exit_code = cmd_clean();
	}
	else if (cmd == '-h' || cmd == '--help' || cmd == 'help') {
		usage();
		exit_code = 0;
	}
	else {
		usage();
		exit_code = 1;
	}
}
catch (e) {
	if (lock_held)
		release_lock();
	if (upgrade_lock_held)
		release_upgrade_lock();
	if (run_mode == 'upgrade')
		upgrade_log('fatal: ' + e);
	else if (run_mode == 'update')
		log('fatal: ' + e);
	exit_code = 6;
}

// Keep the update and upgrade exit markers separate so the two UI pages can
// poll their own log file.
if (run_mode == 'update')
	log(sprintf('UPDATE_EXITED %d', exit_code));
else if (run_mode == 'upgrade' && !upgrade_marker_written)
	upgrade_log(sprintf('UPGRADE_EXITED %d', exit_code));

exit(exit_code);
