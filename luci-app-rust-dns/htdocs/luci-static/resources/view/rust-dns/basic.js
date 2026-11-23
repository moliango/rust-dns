'use strict';
/*
 * SPDX-License-Identifier: GPL-3.0-only
 *
 * LuCI basic settings page for rust-dns.
 *
 * The page only writes UCI. The rust-dns init script (owned by the service
 * package) turns UCI into the runtime config and restarts procd. The custom
 * YAML editor is the single exception: it validates the file with
 * `rust-dns check -c <tmp>` before installing it.
 *
 * Layout and field names are adapted from luci-app-mosdns
 * (htdocs/luci-static/resources/view/mosdns/basic.js),
 * Copyright (C) 2023-2026 sbwml <admin@cooluc.com>, GPL-3.0.
 */

'require form';
'require fs';
'require poll';
'require rpc';
'require ui';
'require view';

const CONFIG_GENERATED = '/var/etc/rust-dns/config.json';
const CONFIG_CUSTOM = '/etc/rust-dns/config_custom.yaml';
const RUST_DNS_BIN = '/usr/bin/rust-dns';
const INIT_SCRIPT = '/etc/init.d/rust-dns';
const LAST_ERROR_FILE = '/var/etc/rust-dns/last-error.log';
const STATUS_POLL_INTERVAL = 5;

const callServiceList = rpc.declare({
	object: 'service',
	method: 'list',
	params: [ 'name' ],
	expect: { '': {} }
});

const callGetVersion = rpc.declare({
	object: 'luci.rust_dns',
	method: 'get_version',
	expect: { '': {} }
});

const callFlushCache = rpc.declare({
	object: 'luci.rust_dns',
	method: 'flush_cache',
	expect: { '': {} }
});

const callImportPreview = rpc.declare({
	object: 'luci.rust_dns',
	method: 'import_mosdns_preview',
	expect: { '': {} }
});

const callImportApply = rpc.declare({
	object: 'luci.rust_dns',
	method: 'import_mosdns_apply',
	expect: { '': {} }
});

const CODEMIRROR_BASE = '/luci-static/resources/codemirror5';
const EDITOR_ELEMENT_ID = 'widget.cbid.rust-dns.config._custom';

let serviceVersion = '';
let statusPollRegistered = false;
let yamlEditor = null;
let codeMirrorLoad = null;

/* Load the vendored CodeMirror assets once. Any failure rejects the promise,
 * and the caller keeps the plain textarea as a fallback. */
function loadCodeMirrorResources() {
	if (codeMirrorLoad)
		return codeMirrorLoad;

	const styles = [
		CODEMIRROR_BASE + '/theme/dracula.min.css',
		CODEMIRROR_BASE + '/addon/lint/lint.min.css',
		CODEMIRROR_BASE + '/codemirror.min.css'
	];
	const scripts = [
		CODEMIRROR_BASE + '/libs/js-yaml.min.js',
		CODEMIRROR_BASE + '/codemirror.min.js',
		CODEMIRROR_BASE + '/addon/display/autorefresh.min.js',
		CODEMIRROR_BASE + '/mode/yaml/yaml.min.js',
		CODEMIRROR_BASE + '/addon/lint/lint.min.js',
		CODEMIRROR_BASE + '/addon/lint/yaml-lint.min.js'
	];

	const addStyle = href => new Promise(resolve => {
		const link = document.createElement('link');
		link.rel = 'stylesheet';
		link.href = href;
		link.onload = resolve;
		link.onerror = resolve;
		document.head.appendChild(link);
	});

	const addScript = src => new Promise((resolve, reject) => {
		const script = document.createElement('script');
		script.src = src;
		script.onload = resolve;
		script.onerror = () => reject(new Error('Failed to load ' + src));
		document.head.appendChild(script);
	});

	codeMirrorLoad = Promise.all(styles.map(addStyle)).then(async () => {
		for (const src of scripts)
			await addScript(src);

		if (typeof window.CodeMirror === 'undefined' || typeof window.CodeMirror.fromTextArea !== 'function')
			throw new Error('CodeMirror is unavailable');
	}).catch(err => {
		codeMirrorLoad = null;
		throw err;
	});

	return codeMirrorLoad;
}

function initYamlEditor(attempt) {
	if (yamlEditor)
		return;

	attempt = attempt || 0;

	const textarea = document.getElementById(EDITOR_ELEMENT_ID);
	if (!textarea) {
		if (attempt < 20)
			window.setTimeout(() => initYamlEditor(attempt + 1), 150);
		return;
	}

	try {
		yamlEditor = window.CodeMirror.fromTextArea(textarea, {
			autoRefresh: true,
			lineNumbers: true,
			lineWrapping: true,
			lint: true,
			gutters: [ 'CodeMirror-lint-markers' ],
			mode: 'text/yaml',
			theme: 'dracula'
		});

		// Keep the underlying textarea in sync so LuCI's normal form
		// serialization sees the edited YAML.
		yamlEditor.on('change', () => {
			if (textarea.value !== yamlEditor.getValue())
				textarea.value = yamlEditor.getValue();
		});
	} catch (e) {
		yamlEditor = null;
		console.warn('rust-dns: CodeMirror initialization failed, using textarea', e);
	}
}

function setupYamlEditor() {
	loadCodeMirrorResources().then(() => initYamlEditor()).catch(err => {
		// The plain textarea remains functional, so a failure is non-fatal.
		console.warn('rust-dns: CodeMirror resources could not be loaded, using textarea', err);
	});
}

function escapeHtml(value) {
	return String(value == null ? '' : value).replace(/[&<>"']/g, ch => ({
		'&': '&amp;',
		'<': '&lt;',
		'>': '&gt;',
		'"': '&quot;',
		"'": '&#39;'
	})[ch]);
}

function getServiceStatus() {
	return L.resolveDefault(callServiceList('rust-dns'), {}).then(res => {
		try {
			const service = res['rust-dns'];
			if (!service)
				return false;

			const instances = service.instances || {};
			for (const name in instances) {
				if (instances[name] && instances[name].running)
					return true;
			}
		} catch (e) {}

		return false;
	});
}

function renderStatus(isRunning) {
	const version = serviceVersion ? ' ' + escapeHtml(serviceVersion) : '';
	const spanTemp = '<em><span style="color:%s"><strong>%s %s</strong></span></em>';

	if (isRunning)
		return spanTemp.format('green', _('Rust DNS') + version, _('RUNNING'));

	return spanTemp.format('red', _('Rust DNS') + version, _('NOT RUNNING'));
}

function refreshStatus() {
	return getServiceStatus().then(isRunning => {
		const node = document.getElementById('service_status');
		if (node)
			node.innerHTML = renderStatus(isRunning);

		const buttons = {
			btn_start: isRunning,
			btn_restart: !isRunning,
			btn_stop: !isRunning
		};

		for (const id in buttons) {
			const button = document.getElementById(id);
			if (button)
				button.disabled = buttons[id];
		}
	});
}

function runServiceCommand(command) {
	return fs.exec(INIT_SCRIPT, [ command ]).then(res => {
		if (res.code !== 0)
			throw new Error((res.stderr || res.stdout || _('Command failed')).trim());

		ui.addNotification(null, E('p', _('Service command "%s" completed.').format(command)), 'info');
	}).catch(err => {
		ui.addNotification(null,
			E('p', _('Service command "%s" failed: %s').format(command, err.message)), 'error');
	}).then(() => refreshStatus());
}

function renderLastError(content) {
	if (!content || content.trim() === '')
		return null;

	return E('div', { class: 'alert-message warning', style: 'margin-bottom: 1em;' }, [
		E('strong', {}, _('Last configuration error')),
		E('pre', {
			style: 'white-space: pre-wrap; word-break: break-word; margin: 0.5em 0 0 0; font-size: 90%;'
		}, content.trim())
	]);
}

function formatImportValue(value) {
	if (Array.isArray(value))
		return value.length ? value.join(', ') : '-';

	if (value == null || value === '')
		return '-';

	return String(value);
}

function importTable(headers, rows) {
	return E('table', { class: 'table', style: 'width: 100%; margin-bottom: 1em;' }, [
		E('thead', {}, E('tr', {}, headers.map(header => E('th', {}, header)))),
		E('tbody', {}, rows)
	]);
}

function importStatusLabel(status) {
	switch (status) {
	case 'new': return _('New');
	case 'changed': return _('Changed');
	case 'copied': return _('Copied');
	case 'unchanged': return _('Unchanged');
	case 'error': return _('Error');
	default: return status || '-';
	}
}

function buildImportPreviewContent(res) {
	const parts = [];
	const mapped = res.mapped || [];
	const rules = res.rules || [];
	const skipped = res.skipped || [];
	const warnings = res.warnings || [];

	parts.push(E('p', {}, _('The following options will be written to rust-dns:')));

	if (mapped.length === 0) {
		parts.push(E('p', {}, _('No compatible MosDNS options were found.')));
	} else {
		parts.push(importTable([
			_('rust-dns option'),
			_('MosDNS option'),
			_('MosDNS value'),
			_('Current rust-dns value')
		], mapped.map(item => E('tr', {}, [
			E('td', {}, item.to),
			E('td', {}, item.from),
			E('td', { style: 'word-break: break-all;' }, formatImportValue(item.value)),
			E('td', { style: 'word-break: break-all;' }, formatImportValue(item.current))
		]))));
	}

	parts.push(E('h4', {}, _('Rule files to copy')));

	if (rules.length === 0) {
		parts.push(E('p', {}, _('No rule files were found.')));
	} else {
		parts.push(importTable([ _('File'), _('Status'), _('Size') ],
			rules.map(rule => E('tr', {}, [
				E('td', {}, rule.name),
				E('td', {}, importStatusLabel(rule.status)),
				E('td', {}, String(rule.size == null ? '' : rule.size))
			]))));
	}

	if (skipped.length > 0) {
		parts.push(E('h4', {}, _('Unmapped options')));
		parts.push(importTable([ _('MosDNS option'), _('MosDNS value'), _('Reason') ],
			skipped.map(item => E('tr', {}, [
				E('td', {}, item.option),
				E('td', { style: 'word-break: break-all;' }, formatImportValue(item.value)),
				E('td', {}, item.reason || '')
			]))));
	}

	if (warnings.length > 0) {
		parts.push(E('h4', {}, _('Warnings')));
		parts.push(E('ul', {}, warnings.map(warning => E('li', {}, warning))));
	}

	parts.push(E('p', { style: 'margin-top: 1em;' },
		_('A backup archive will be written to %s before any change. MosDNS data is not modified.')
			.format((res.backup_prefix || '/etc/rust-dns/import-backup-') + '<timestamp>.tar.gz')));
	parts.push(E('p', { style: 'font-weight: bold;' },
		_('Nothing will be written until you confirm the import.')));

	return parts;
}

return view.extend({
	load() {
		return Promise.all([
			L.resolveDefault(callGetVersion(), {}),
			L.resolveDefault(fs.read(LAST_ERROR_FILE), '')
		]);
	},

	handleFlushCache() {
		return callFlushCache().then(res => {
			if (res && res.success) {
				ui.addNotification(null, E('p', _('DNS cache flushed successfully.')), 'info');
			} else {
				ui.addNotification(null,
					E('p', _('Failed to flush the DNS cache.') +
						(res && res.error ? ': ' + res.error : '')), 'error');
			}
		}).catch(err => {
			ui.addNotification(null, E('p', _('Failed to flush the DNS cache: %s').format(err.message)), 'error');
		});
	},

	handleImportMosdns() {
		ui.showModal(_('Import from MosDNS'), [
			E('p', { class: 'spinning' }, _('Reading MosDNS configuration...'))
		]);

		return callImportPreview().then(res => {
			ui.hideModal();

			if (!res) {
				ui.addNotification(null, E('p',
					_('Failed to read the MosDNS import preview: %s').format(_('Unknown error'))), 'error');
				return;
			}

			// `available === false` means there is nothing to import, but it can
			// be a missing installation or an unreadable/invalid configuration.
			// Surface the concrete backend error when present; only fall back to
			// the translated "not installed" notice when there is none.
			if (res.available === false || res.error) {
				if (res.error) {
					ui.addNotification(null, E('p',
						_('Failed to read the MosDNS import preview: %s').format(res.error)), 'error');
				} else {
					ui.addNotification(null, E('p',
						_('No MosDNS configuration was found at %s. Nothing was imported.')
							.format('/etc/config/mosdns')), 'warning');
				}
				return;
			}

			this.showImportPreview(res);
		}).catch(err => {
			ui.hideModal();
			ui.addNotification(null,
				E('p', _('Failed to read the MosDNS import preview: %s').format(err.message)), 'error');
		});
	},

	showImportPreview(res) {
		const mapped = res.mapped || [];
		const rules = res.rules || [];
		const nothingToImport = (mapped.length === 0 && rules.length === 0);

		const applyButton = E('button', {
			'class': 'btn cbi-button cbi-button-apply',
			'disabled': 'disabled',
			'click': () => this.runImportApply()
		}, _('Import and reload rust-dns'));

		const confirm = E('input', {
			'type': 'checkbox',
			'change': ev => { applyButton.disabled = nothingToImport || !ev.target.checked; }
		});

		ui.showModal(_('MosDNS import preview'), [
			E('div', { style: 'max-height: 60vh; overflow: auto;' }, buildImportPreviewContent(res)),
			E('label', { style: 'display: block; margin-top: 1em;' }, [
				confirm,
				' ',
				_('I understand the selected MosDNS settings and rule files will be written to rust-dns.')
			]),
			E('div', { class: 'right' }, [
				E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
				' ',
				applyButton
			])
		]);
	},

	runImportApply() {
		ui.showModal(_('Importing MosDNS configuration'), [
			E('p', { class: 'spinning' }, _('Please wait, this may take a few moments...'))
		]);

		return callImportApply().then(res => {
			ui.hideModal();

			if (!res || res.success !== true) {
				ui.addNotification(null, E('p',
					_('MosDNS import failed: %s')
						.format(res && res.error ? res.error : _('Unknown error'))), 'error');
				return;
			}

			const lines = [];

			const importedCount = (res.imported || []).length;
			const importedRules = (res.rules || []).filter(rule => rule.status !== 'error').length;

			lines.push(E('p', {}, _('Imported %d option(s) and %d rule file(s).')
				.format(importedCount, importedRules)));

			if (res.backup)
				lines.push(E('p', {}, _('Backup written to %s.').format(res.backup)));

			const warnings = res.warnings || [];
			for (const warning of warnings)
				lines.push(E('p', { style: 'color: #b35900;' }, warning));

			if (res.reload && res.reload.code !== 0)
				lines.push(E('p', { style: 'color: #b35900;' },
					_('The rust-dns service could not be reloaded after the import. Restart it manually.')));

			// Keep the result visible; the page is reloaded only when the user
			// closes the dialog so the imported UCI values are shown.
			ui.showModal(_('MosDNS import completed.'), [
				E('div', { style: 'max-height: 50vh; overflow: auto;' }, lines),
				E('div', { class: 'right' }, [
					E('button', {
						'class': 'btn',
						'click': () => {
							ui.hideModal();
							window.location.reload();
						}
					}, _('Close'))
				])
			]);
		}).catch(err => {
			ui.hideModal();
			ui.addNotification(null, E('p', _('MosDNS import failed: %s').format(err.message)), 'error');
		});
	},

	render(data) {
		let m, s, o;

		const versionData = data[0] || {};
		const lastError = data[1] || '';

		serviceVersion = versionData.version || '';

		m = new form.Map('rust-dns', _('Rust DNS') + (serviceVersion ? ' ' + serviceVersion : ''),
			_('rust-dns is a Rust rewrite of mosdns with compatible rule files and OpenWrt integration.'));

		s = m.section(form.TypedSection);
		s.anonymous = true;
		s.render = () => {
			setTimeout(() => {
				if (!statusPollRegistered) {
					statusPollRegistered = true;
					poll.add(() => refreshStatus(), STATUS_POLL_INTERVAL);
				}
				refreshStatus();
				setupYamlEditor();
			}, 100);

			const children = [
				E('p', { id: 'service_status' }, _('Collecting data...')),
				E('div', { class: 'cbi-section-actions' }, [
					E('button', {
						'class': 'btn cbi-button cbi-button-apply',
						'id': 'btn_start',
						'type': 'button',
						'click': ev => { ev.preventDefault(); return runServiceCommand('start'); }
					}, _('Start')),
					E('button', {
						'class': 'btn cbi-button cbi-button-reset',
						'id': 'btn_stop',
						'type': 'button',
						'click': ev => { ev.preventDefault(); return runServiceCommand('stop'); }
					}, _('Stop')),
					E('button', {
						'class': 'btn cbi-button cbi-button-action',
						'id': 'btn_restart',
						'type': 'button',
						'click': ev => { ev.preventDefault(); return runServiceCommand('restart'); }
					}, _('Restart')),
					E('button', {
						'class': 'btn cbi-button cbi-button-action',
						'type': 'button',
						'click': ev => { ev.preventDefault(); return runServiceCommand('enable'); }
					}, _('Enable autostart')),
					E('button', {
						'class': 'btn cbi-button cbi-button-action',
						'type': 'button',
						'click': ev => { ev.preventDefault(); return runServiceCommand('disable'); }
					}, _('Disable autostart'))
				])
			];

			const lastErrorNode = renderLastError(lastError);
			if (lastErrorNode)
				children.push(lastErrorNode);

			return E('div', { class: 'cbi-section', id: 'status_bar' }, children);
		};

		s = m.section(form.NamedSection, 'config', 'rust-dns');

		s.tab('basic', _('Basic Options'));
		s.tab('advanced', _('Advanced Options'));
		s.tab('cloudflare', _('Cloudflare Options'));
		s.tab('api', _('API Options'));
		s.tab('geodata', _('GeoData Export'));

		/* Basic */

		o = s.taboption('basic', form.Flag, 'enabled', _('Enabled'));
		o.default = false;
		o.rmempty = false;

		o = s.taboption('basic', form.ListValue, 'configfile', _('Config File'));
		o.value(CONFIG_GENERATED, _('Generated Config'));
		o.value(CONFIG_CUSTOM, _('Custom Config'));
		o.default = CONFIG_GENERATED;
		o.rmempty = false;

		o = s.taboption('basic', form.Value, 'listen_port', _('Listen Port'));
		o.default = '5335';
		o.datatype = 'port';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.Value, 'listen_address', _('Listen Address'));
		o.default = '0.0.0.0';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.ListValue, 'log_level', _('Log Level'));
		o.value('debug', _('Debug'));
		o.value('info', _('Info'));
		o.value('warn', _('Warning'));
		o.value('error', _('Error'));
		o.default = 'error';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.Value, 'log_file', _('Log File'));
		o.placeholder = '/var/log/rust-dns.log';
		o.default = '/var/log/rust-dns.log';
		o.depends('configfile', CONFIG_GENERATED);
		o.rmempty = false;

		o = s.taboption('basic', form.Value, 'log_size', _('Log File Size'),
			_('Maximum size of the log file (in MB).'));
		o.datatype = 'uinteger';
		o.default = '1';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.Value, 'log_backups', _('Log Backups'),
			_('Number of rotated log files to keep. 0 disables rotation.'));
		o.datatype = 'and(uinteger,max(9))';
		o.default = '2';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.Flag, 'redirect', _('DNS Forward'),
			_('Forward dnsmasq queries to rust-dns.'));
		o.default = false;

		if (L.hasSystemFeature('firewall4')) {
			o = s.taboption('basic', form.Flag, 'local_dns_redirect', _('DNS redirect'),
				_('Force all local DNS queries to rust-dns (DNS hijacking).'));
			o.default = false;
			o.depends('redirect', '1');
		}

		o = s.taboption('basic', form.Flag, 'prefer_ipv4_cn', _('China DNS prefer IPv4'),
			_('Prefer IPv4 answers for China DNS resolution of dual-stack domains.'));
		o.depends('configfile', CONFIG_GENERATED);
		o.default = false;

		o = s.taboption('basic', form.Flag, 'prefer_ipv4', _('Remote DNS prefer IPv4'),
			_('Prefer IPv4 answers for Remote / Streaming DNS resolution of dual-stack domains.'));
		o.depends('configfile', CONFIG_GENERATED);
		o.default = false;

		o = s.taboption('basic', form.Flag, 'custom_local_dns', _('Custom China DNS'),
			_('Follow the WAN interface DNS servers unless a custom China DNS is configured.'));
		o.depends('configfile', CONFIG_GENERATED);
		o.default = false;

		o = s.taboption('basic', form.DynamicList, 'local_dns', _('China DNS server'));
		o.value('119.29.29.29', _('Tencent Public DNS (119.29.29.29)'));
		o.value('119.28.28.28', _('Tencent Public DNS (119.28.28.28)'));
		o.value('223.5.5.5', _('Aliyun Public DNS (223.5.5.5)'));
		o.value('223.6.6.6', _('Aliyun Public DNS (223.6.6.6)'));
		o.value('180.184.1.1', _('TrafficRoute Public DNS (180.184.1.1)'));
		o.value('180.184.2.2', _('TrafficRoute Public DNS (180.184.2.2)'));
		o.value('114.114.114.114', _('Xinfeng Public DNS (114.114.114.114)'));
		o.value('114.114.115.115', _('Xinfeng Public DNS (114.114.115.115)'));
		o.value('180.76.76.76', _('Baidu Public DNS (180.76.76.76)'));
		o.value('https://doh.pub/dns-query', _('Tencent Public DNS (DNS over HTTPS)'));
		o.value('quic://dns.alidns.com', _('Aliyun Public DNS (DNS over QUIC)'));
		o.value('https://dns.alidns.com/dns-query', _('Aliyun Public DNS (DNS over HTTPS)'));
		o.value('h3://dns.alidns.com/dns-query', _('Aliyun Public DNS (DNS over HTTP/3)'));
		o.value('https://doh.360.cn/dns-query', _('360 Public DNS (DNS over HTTPS)'));
		o.default = '119.29.29.29';
		o.depends('custom_local_dns', '1');

		o = s.taboption('basic', form.DynamicList, 'remote_dns', _('Remote DNS server'));
		o.value('tls://1.1.1.1', _('Cloudflare Public DNS (1.1.1.1)'));
		o.value('tls://1.0.0.1', _('Cloudflare Public DNS (1.0.0.1)'));
		o.value('tls://8.8.8.8', _('Google Public DNS (8.8.8.8)'));
		o.value('tls://8.8.4.4', _('Google Public DNS (8.8.4.4)'));
		o.value('tls://9.9.9.9', _('Quad9 Public DNS (9.9.9.9)'));
		o.value('tls://149.112.112.112', _('Quad9 Public DNS (149.112.112.112)'));
		o.value('tls://208.67.222.222', _('Cisco Public DNS (208.67.222.222)'));
		o.value('tls://208.67.220.220', _('Cisco Public DNS (208.67.220.220)'));
		o.default = 'tls://8.8.8.8';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('basic', form.Flag, 'custom_stream_media_dns', _('Custom Stream Media DNS'),
			_('Streaming media rules use this DNS server.'));
		o.depends('configfile', CONFIG_GENERATED);
		o.default = false;

		o = s.taboption('basic', form.DynamicList, 'stream_media_dns', _('Streaming Media DNS server'));
		o.value('tls://1.1.1.1', _('Cloudflare Public DNS (1.1.1.1)'));
		o.value('tls://1.0.0.1', _('Cloudflare Public DNS (1.0.0.1)'));
		o.value('tls://8.8.8.8', _('Google Public DNS (8.8.8.8)'));
		o.value('tls://8.8.4.4', _('Google Public DNS (8.8.4.4)'));
		o.value('tls://9.9.9.9', _('Quad9 Public DNS (9.9.9.9)'));
		o.value('tls://149.112.112.112', _('Quad9 Public DNS (149.112.112.112)'));
		o.value('tls://208.67.222.222', _('Cisco Public DNS (208.67.222.222)'));
		o.value('tls://208.67.220.220', _('Cisco Public DNS (208.67.220.220)'));
		o.default = 'tls://8.8.8.8';
		o.depends('custom_stream_media_dns', '1');

		o = s.taboption('basic', form.Value, 'bootstrap_dns', _('Bootstrap DNS servers'),
			_('Bootstrap DNS servers resolve the IP addresses of DoH/DoT upstreams.'));
		o.value('119.29.29.29', _('Tencent Public DNS (119.29.29.29)'));
		o.value('119.28.28.28', _('Tencent Public DNS (119.28.28.28)'));
		o.value('223.5.5.5', _('Aliyun Public DNS (223.5.5.5)'));
		o.value('223.6.6.6', _('Aliyun Public DNS (223.6.6.6)'));
		o.value('114.114.114.114', _('Xinfeng Public DNS (114.114.114.114)'));
		o.value('114.114.115.115', _('Xinfeng Public DNS (114.114.115.115)'));
		o.value('180.76.76.76', _('Baidu Public DNS (180.76.76.76)'));
		o.value('8.8.8.8', _('Google Public DNS (8.8.8.8)'));
		o.value('1.1.1.1', _('Cloudflare Public DNS (1.1.1.1)'));
		o.default = '119.29.29.29';
		o.depends('configfile', CONFIG_GENERATED);

		/* Custom YAML editor. The form uses a plain textarea; setupYamlEditor()
		 * upgrades it to CodeMirror with YAML syntax highlighting and linting
		 * when the vendored assets load. Any failure keeps the textarea. */
		o = s.taboption('basic', form.TextValue, '_custom', _('Configuration Editor'),
			_('Contents of %s. The file is validated with "rust-dns check" before it replaces the current configuration.')
				.format(CONFIG_CUSTOM));
		o.rows = 25;
		o.depends('configfile', CONFIG_CUSTOM);
		o.cfgvalue = () => fs.trimmed(CONFIG_CUSTOM).catch(() => '');
		o.write = async function(section_id, formvalue) {
			// CodeMirror does not update the hidden textarea during form
			// serialization on all LuCI versions, so prefer the editor value.
			const editorValue = yamlEditor ? yamlEditor.getValue() : formvalue;
			const content = (editorValue || '').trim();
			if (content === '')
				return;

			const normalized = content.replace(/\r\n/g, '\n') + '\n';
			// The temp file is written next to the real file so the service can
			// validate it without touching the live configuration. It is left in
			// place on failure and overwritten by the next save attempt.
			const tmpFile = CONFIG_CUSTOM + '.tmp';

			try {
				await fs.write(tmpFile, normalized);

				const check = await fs.exec(RUST_DNS_BIN, [ 'check', '-c', tmpFile ]);
				if (check.code !== 0) {
					const detail = (check.stderr || check.stdout || '').trim();
					throw new Error(detail || _('rust-dns rejected the configuration.'));
				}

				await fs.write(CONFIG_CUSTOM, normalized);

				try {
					const restart = await fs.exec(INIT_SCRIPT, [ 'restart' ]);
					if (restart.code !== 0) {
						ui.addNotification(null,
							E('p', _('Configuration saved, but restarting the service failed: %s')
								.format((restart.stderr || restart.stdout || '').trim())), 'warning');
					}
				} catch (e) {
					ui.addNotification(null,
						E('p', _('Configuration saved, but restarting the service failed: %s').format(e.message)),
						'warning');
				}
			} catch (err) {
				ui.addNotification(null, E('p', _('Configuration validation failed: %s').format(err.message)), 'error');
				throw err;
			}
		};

		/* One-click import from an existing MosDNS installation. The preview
		 * shows the mapping and requires an explicit confirmation before
		 * anything is written. */
		o = s.taboption('basic', form.Button, '_import_mosdns', _('Import from MosDNS'),
			_('Preview and copy compatible settings and rule files from an existing MosDNS installation. A backup archive is created first and MosDNS data is never modified.'));
		o.inputtitle = _('Import from MosDNS');
		o.inputstyle = 'apply';
		o.onclick = L.bind(this.handleImportMosdns, this);

		/* Advanced */

		o = s.taboption('advanced', form.Value, 'concurrent', _('Concurrent'),
			_('Number of upstream DNS servers that may be queried at the same time.'));
		o.datatype = 'and(uinteger,min(1),max(3))';
		o.default = '2';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Value, 'idle_timeout', _('Idle Timeout'),
			_('DoH/TCP/DoT connection multiplexing idle timeout (seconds).'));
		o.datatype = 'and(uinteger,min(1))';
		o.default = '30';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'enable_pipeline', _('TCP/DoT Connection Multiplexing'),
			_('Enable RFC 7766 query pipelining for TCP/DoT connections.'));
		o.rmempty = false;
		o.default = true;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'insecure_skip_verify', _('Disable TLS Certificate Check'),
			_('Skip TLS certificate validation for upstream servers. Use only when the system time or CA store is broken.'));
		o.rmempty = false;
		o.default = false;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'enable_ecs_remote', _('Enable EDNS Client Subnet'));
		o.rmempty = false;
		o.default = false;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Value, 'remote_ecs_ip', _('ECS IP Address'),
			_('IP address used for remote upstream ECS. The /24 subnet is sent to the upstream.'));
		o.datatype = 'ipaddr';
		o.depends('enable_ecs_remote', '1');

		o = s.taboption('advanced', form.Flag, 'dns_leak', _('Prevent DNS Leaks'),
			_('Force the fallback policy to use the remote DNS upstream.'));
		o.rmempty = false;
		o.default = false;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'cache', _('Enable DNS Cache'));
		o.rmempty = false;
		o.default = true;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Value, 'cache_size', _('DNS Cache Size'));
		o.datatype = 'and(uinteger,min(0))';
		o.default = '8000';
		o.depends('cache', '1');

		o = s.taboption('advanced', form.Value, 'lazy_cache_ttl', _('Lazy Cache TTL'),
			_('How long expired cache entries stay usable (seconds). Set to 0 to disable lazy caching.'));
		o.datatype = 'and(uinteger,min(0))';
		o.default = '86400';
		o.depends('cache', '1');

		o = s.taboption('advanced', form.Flag, 'prefetch', _('Cache Prefetching'),
			_('Refresh hot cache entries in the background before they expire.'));
		o.rmempty = false;
		o.default = false;
		o.depends('cache', '1');

		o = s.taboption('advanced', form.Value, 'prefetch_before_expire', _('Prefetch Before Expire'),
			_('Prefetch when the remaining TTL is below this value (seconds).'));
		o.datatype = 'and(uinteger,min(1))';
		o.default = '10';
		o.depends('prefetch', '1');

		o = s.taboption('advanced', form.Value, 'prefetch_min_hits', _('Prefetch Min Hits'),
			_('Minimum cache hits since the last refresh before a prefetch is triggered.'));
		o.datatype = 'and(uinteger,min(1))';
		o.default = '3';
		o.depends('prefetch', '1');

		o = s.taboption('advanced', form.Value, 'prefetch_scan_interval', _('Prefetch Scan Interval'),
			_('Interval between background cache scans (seconds).'));
		o.datatype = 'and(uinteger,min(1))';
		o.default = '5';
		o.depends('prefetch', '1');

		o = s.taboption('advanced', form.Value, 'dump_interval', _('Auto Save Cache Interval'),
			_('Interval for automatic cache dumps (seconds). Set to 0 to disable. Dumps are written to /var/rust-dns/cache.dump.'));
		o.datatype = 'and(uinteger,min(0))';
		o.default = '3600';
		o.depends('cache', '1');

		o = s.taboption('advanced', form.Value, 'minimal_ttl', _('Minimum TTL'),
			_('Clamp the minimum DNS answer TTL (seconds). 0 disables clamping.'));
		o.datatype = 'and(uinteger,min(0),max(604800))';
		o.default = '0';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Value, 'maximum_ttl', _('Maximum TTL'),
			_('Clamp the maximum DNS answer TTL (seconds). 0 disables clamping.'));
		o.datatype = 'and(uinteger,min(0),max(604800))';
		o.default = '0';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'reject_type65', _('Disable RR Type 65 (HTTPS/SVCB)'),
			_('Block HTTPS/SVCB records and force A/AAAA answers only.'));
		o.default = false;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('advanced', form.Flag, 'adblock', _('Enable DNS Adblock'));
		o.depends('configfile', CONFIG_GENERATED);
		o.default = false;

		o = s.taboption('advanced', form.DynamicList, 'ad_source', _('Adblock Source'),
			_('Local files and domain lists are supported, for example file:///var/rust-dns/example.txt.'));
		o.depends('adblock', '1');
		o.default = 'geosite.dat';
		o.value('geosite.dat', 'v2ray-geosite');
		o.value('https://raw.githubusercontent.com/privacy-protection-tools/anti-AD/master/anti-ad-domains.txt', 'anti-AD');
		o.value('https://raw.githubusercontent.com/Cats-Team/AdRules/main/mosdns_adrules.txt', 'Cats-Team/AdRules');
		o.value('https://raw.githubusercontent.com/neodevpro/neodevhost/master/domain', 'NEO DEV HOST');

		/* Cloudflare */

		o = s.taboption('cloudflare', form.Flag, 'cloudflare', _('Enabled'),
			_('Rewrite answers that match Cloudflare IP ranges to a custom IP (experimental).'));
		o.rmempty = false;
		o.default = false;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('cloudflare', form.DynamicList, 'cloudflare_ip', _('Custom IP'));
		o.datatype = 'ipaddr';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('cloudflare', form.DummyValue, '_cloudflare_ranges', _('Cloudflare IP Ranges'));
		o.rawhtml = true;
		o.depends('configfile', CONFIG_GENERATED);
		o.cfgvalue = () => _('The CIDR list is maintained by the rust-dns service package in %s and can be refreshed by the Cloudflare optimizer.')
			.format('/etc/rust-dns/rule/cloudflare-cidr.txt');

		/* API */

		o = s.taboption('api', form.Value, 'listen_port_api', _('API Listen Port'));
		o.datatype = 'port';
		o.default = '9091';
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('api', form.Value, 'api_listen_address', _('API Listen Address'),
			_('Address the admin API binds to. v1 only supports loopback addresses; keep 127.0.0.1 unless you know what you are doing.'));
		o.datatype = 'ipaddr';
		o.default = '127.0.0.1';
		o.validate = function(section_id, value) {
			if (!value || value === '')
				return true;

			const normalized = String(value).replace(/^\[|\]$/g, '');

			if (normalized === '::1' || /^127\.\d{1,3}\.\d{1,3}\.\d{1,3}$/.test(normalized))
				return true;

			return _('The admin API must bind to a loopback address (127.0.0.1 or ::1).');
		};
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('api', form.Flag, 'stats_collector', _('Enable Stats Collector'));
		o.rmempty = false;
		o.default = true;
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('api', form.Value, 'stats_capacity', _('Ring Buffer Capacity'),
			_('Query log ring buffer capacity (FIFO overwrite). Larger values use more memory.'));
		o.datatype = 'and(uinteger,min(100),max(100000))';
		o.default = '2000';
		o.depends('stats_collector', '1');

		o = s.taboption('api', form.Value, 'stats_history_max', _('History Points'),
			_('Maximum number of history points kept for the sparklines.'));
		o.datatype = 'and(uinteger,min(1),max(168))';
		o.default = '168';
		o.depends('stats_collector', '1');

		o = s.taboption('api', form.Value, 'stats_top_max', _('Top List Size'),
			_('Maximum number of entries returned for each Top list.'));
		o.datatype = 'and(uinteger,min(1),max(1000))';
		o.default = '100';
		o.depends('stats_collector', '1');

		o = s.taboption('api', form.Value, 'stats_logs_max', _('Query Log Limit'),
			_('Maximum page size accepted by the query log API.'));
		o.datatype = 'and(uinteger,min(1),max(100000))';
		o.default = '200';
		o.depends('stats_collector', '1');

		o = s.taboption('api', form.Flag, 'stats_dump_file', _('Stats Dump'),
			_('Persist query statistics and logs and reload them on the next startup.'));
		o.rmempty = false;
		o.default = false;
		o.depends('stats_collector', '1');

		o = s.taboption('api', form.Value, 'stats_dump_interval', _('Auto Save Stats Interval'));
		o.datatype = 'and(uinteger,min(60),max(86400))';
		o.default = '600';
		o.depends('stats_dump_file', '1');

		o = s.taboption('api', form.Value, 'stats_dump_path', _('Stats Dump File'),
			_('File used to persist the statistics ring buffer.'));
		o.default = '/var/rust-dns/stats.dump';
		o.depends('stats_dump_file', '1');

		o = s.taboption('api', form.DummyValue, '_api_notice', _('Security Notice'));
		o.rawhtml = true;
		o.cfgvalue = () => _('The admin API is bound to 127.0.0.1 by the rust-dns service. Do not expose the API port to untrusted networks.');

		o = s.taboption('api', form.Button, '_flush_cache', null,
			_('Flushing the cache removes all cached DNS records.'));
		o.title = '&#160;';
		o.inputtitle = _('Flush DNS Cache');
		o.inputstyle = 'apply';
		o.onclick = L.bind(this.handleFlushCache, this);
		o.depends('cache', '1');

		/* GeoData */

		o = s.taboption('geodata', form.DynamicList, 'geosite_tags', _('GeoSite Tags'),
			_('GeoSite categories to export. Multiple tags are allowed.'),
			_('Export directory: /var/rust-dns'));
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('geodata', form.DynamicList, 'geoip_tags', _('GeoIP Tags'),
			_('GeoIP categories to export. Multiple tags are allowed.'),
			_('Export directory: /var/rust-dns'));
		o.depends('configfile', CONFIG_GENERATED);

		o = s.taboption('geodata', form.DummyValue, '_geodata_notice', _('GeoData Notice'));
		o.rawhtml = true;
		o.cfgvalue = () => _('GeoData export is performed by the rust-dns update tool from v2ray-geosite/v2ray-geoip data. The Update page manages the source archives.');

		return m.render();
	}
});
