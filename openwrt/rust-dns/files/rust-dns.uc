#!/usr/bin/ucode
// SPDX-License-Identifier: GPL-3.0-only
//
// rust-dns geodata / adlist update tool.
//
//   ucode /usr/share/rust-dns/rust-dns.uc update [--all|--geosite|--geoip|--adlist]
//   ucode /usr/share/rust-dns/rust-dns.uc status
//   ucode /usr/share/rust-dns/rust-dns.uc clean
//
// Design contract (11-23-rust-dns-service/design.md section 9):
//   * atomic mkdir lock in /var/lock; a live owner means exit code 2;
//   * all downloads and conversions happen in /var/rust-dns/.staging.<pid>;
//   * existing files are only replaced after validation succeeds;
//   * one generation of backups is kept in /var/rust-dns/.bak;
//   * the previous data is never removed on failure;
//   * every run appends "UPDATE_EXITED <code>" to /var/rust-dns/update.log.
//
// Exit codes: 0 ok, 2 lock held, 3 download failure, 4 validation failure,
//             5 conversion failure, 6 IO/permission/other.

'use strict';

import * as fs from 'fs';
import * as uci from 'uci';

const DATA_DIR = '/var/rust-dns';
const LOCK_DIR = '/var/lock/rust-dns_update.lock';
const LOG_FILE = DATA_DIR + '/update.log';
const MANIFEST = DATA_DIR + '/manifest.json';
const BACKUP_DIR = DATA_DIR + '/.bak';
const MAX_LOG = 262144;

let lock_held = false;

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

function log(msg) {
	run('mkdir -p ' + shq(DATA_DIR));
	if (file_size(LOG_FILE) > MAX_LOG)
		run('tail -c ' + (MAX_LOG / 2) + ' ' + shq(LOG_FILE) + ' > ' + shq(LOG_FILE + '.tmp') +
		    ' && mv -f ' + shq(LOG_FILE + '.tmp') + ' ' + shq(LOG_FILE));

	let line = sprintf('%s %s\n', timestamp(), msg);
	printf('%s', line);
	let handle = null;
	try {
		handle = fs.open(LOG_FILE, 'a');
	}
	catch (e) {
		return;
	}
	if (handle) {
		handle.write(line);
		handle.close();
	}
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

// All v2dat_dump invocation details live here so tests can substitute a fake
// binary on PATH.
function v2dat_dump(datfile, tag, outfile) {
	run('rm -f ' + shq(outfile));
	let rc = run('v2dat_dump ' + shq(datfile) + ' ' + shq(tag) + ' > ' + shq(outfile) + ' 2>/dev/null');
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
	let dat = null;

	if (is_true(opt('geo_auto_update', 0)))
		dat = download_dat(kind, staging);
	if (dat == null)
		dat = installed_dat(kind);

	if (dat == null) {
		log(kind + ': no .dat source available');
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
		let out = sprintf('%s/%s_%s.txt', staging, kind, tag);
		if (v2dat_dump(dat, tag, out))
			produced = true;
		else
			log(sprintf('%s: v2dat_dump failed for tag %s', kind, tag));
	}

	// The generated config references the fixed *_cn.txt paths; make sure they
	// exist even when the user only selected custom tags.
	let cn = sprintf('%s/%s_cn.txt', staging, kind);
	if (!file_exists(cn) && v2dat_dump(dat, 'cn', cn))
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

function usage() {
	printf('Usage: rust-dns.uc update [--all|--geosite|--geoip|--adlist]\n');
	printf('       rust-dns.uc status\n');
	printf('       rust-dns.uc clean\n');
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

let exit_code = 0;

try {
	let args = ARGV ?? [];
	let cmd = args[0] ?? 'status';

	if (cmd == 'update') {
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
	log('fatal: ' + e);
	exit_code = 6;
}

log(sprintf('UPDATE_EXITED %d', exit_code));
exit(exit_code);
