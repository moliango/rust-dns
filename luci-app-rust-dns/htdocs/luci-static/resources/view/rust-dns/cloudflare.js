'use strict';
'require poll';
'require rpc';
'require ui';
'require view';

/*
 * SPDX-License-Identifier: GPL-3.0-only
 *
 * Cloudflare IP optimizer page.
 *
 * The page is a thin frontend for the rpcd methods cf_start / cf_cancel /
 * cf_status / cf_results / cf_apply_hosts / cf_clear. All paths are fixed in
 * the backend; this view only shows them. Progress is read from
 * /var/rust-dns/cf-optimizer/progress.json by the backend and polled once per
 * second. The page must load even when the rust-dns binary or optimizer
 * subcommand is missing: every RPC failure is rendered as an error banner.
 */

const CF_DIR = '/var/rust-dns/cf-optimizer';
const HOSTS_FILE = '/etc/rust-dns/rule/hosts.txt';

const DEFAULTS = {
	source: 'cloudflare',
	ip_file: CF_DIR + '/ips.txt',
	family: '4',
	port: 443,
	threads: 50,
	ping: 4,
	download_count: 5,
	download_time: 10,
	download_bytes: 10000000,
	download_concurrency: 4,
	url: 'https://speed.cloudflare.com/__down?bytes=10000000',
	max_candidates: 1000,
	max_latency: 300,
	max_loss: 20,
	min_speed: 1,
	bypass_mode: 'none',
	mark: '0x400000',
	device: 'wan',
	source_ip: '',
	dry_run: false,
	output: CF_DIR + '/last.json',
	progress: CF_DIR + '/progress.json',
	cancel: CF_DIR + '/cancel'
};

const callCfStart = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_start',
	params: [ 'config' ],
	expect: { '': {} }
});

const callCfCancel = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_cancel',
	expect: { '': {} }
});

const callCfStatus = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_status',
	expect: { '': {} }
});

const callCfResults = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_results',
	expect: { '': {} }
});

const callCfApplyHosts = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_apply_hosts',
	params: [ 'domains', 'ips' ],
	expect: { '': {} }
});

const callCfClear = rpc.declare({
	object: 'luci.rust_dns',
	method: 'cf_clear',
	expect: { '': {} }
});

const state = {
	el: null,
	status: null,
	report: null,
	lastState: null,
	sortKey: 'speed_mbps',
	sortDir: 'desc',
	filter: '',
	passedFilter: 'all',
	selected: {},
	busy: false,
	refreshInFlight: false,
	pollRegistered: false
};

function safeCall(fn, fallback) {
	try {
		return L.resolveDefault(fn(), fallback).catch(e => ({
			error: (e && e.message) ? e.message : String(e)
		}));
	}
	catch (e) {
		return Promise.resolve({ error: (e && e.message) ? e.message : String(e) });
	}
}

function note(level, message) {
	ui.addNotification(null, E('p', {}, message), level);
}

function styleSheet() {
	if (document.getElementById('rust-dns-cloudflare-styles'))
		return;

	const css = [
		'.cf-section { margin-bottom: 1.4rem; }',
		'.cf-section h3 { margin: 0 0 0.75rem 0; }',
		'.cf-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(230px, 1fr)); gap: 0.75rem 1rem; }',
		'.cf-field { display: flex; flex-direction: column; gap: 0.2rem; min-width: 0; }',
		'.cf-field > label { font-weight: 600; font-size: 0.85rem; }',
		'.cf-field input, .cf-field select, .cf-field textarea { width: 100%; box-sizing: border-box; }',
		'.cf-field input[readonly] { opacity: 0.75; }',
		'.cf-hint { font-size: 0.78rem; opacity: 0.7; line-height: 1.35; }',
		'.cf-actions { display: flex; flex-wrap: wrap; gap: 0.5rem; align-items: center; margin-bottom: 0.75rem; }',
		'.cf-progress { height: 10px; background: rgba(125,125,125,0.18); border-radius: 5px; overflow: hidden; margin: 0.5rem 0; }',
		'.cf-progress > div { height: 100%; width: 0; background: #3b82f6; transition: width 0.3s ease; }',
		'.cf-status { display: flex; flex-wrap: wrap; gap: 0.4rem 1rem; align-items: baseline; font-size: 0.85rem; }',
		'.cf-status strong { font-size: 0.95rem; }',
		'.cf-warning { border-left: 4px solid #d97706; background: rgba(217,119,6,0.10); padding: 0.5rem 0.75rem; border-radius: 4px; margin-bottom: 0.4rem; font-size: 0.85rem; }',
		'.cf-error { border-left: 4px solid #dc2626; background: rgba(220,38,38,0.10); padding: 0.5rem 0.75rem; border-radius: 4px; margin-bottom: 0.4rem; font-size: 0.85rem; }',
		'.cf-info { border-left: 4px solid #2563eb; background: rgba(37,99,235,0.10); padding: 0.5rem 0.75rem; border-radius: 4px; margin-bottom: 0.4rem; font-size: 0.85rem; }',
		'.cf-toolbar { display: flex; flex-wrap: wrap; gap: 0.5rem; align-items: center; margin: 0.5rem 0; }',
		'.cf-toolbar input[type=text] { min-width: 180px; }',
		'.cf-table-wrap { overflow-x: auto; -webkit-overflow-scrolling: touch; }',
		'.cf-table { width: 100%; border-collapse: collapse; font-size: 0.85rem; }',
		'.cf-table th, .cf-table td { padding: 0.42rem 0.55rem; border-bottom: 1px solid rgba(125,125,125,0.15); text-align: left; white-space: nowrap; }',
		'.cf-table th.sortable { cursor: pointer; user-select: none; }',
		'.cf-table th.sortable:hover { background: rgba(125,125,125,0.06); }',
		'.cf-table td.cf-empty { text-align: center; opacity: 0.65; padding: 1.5rem 0; }',
		'.cf-mono { font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; }',
		'.cf-badge { display: inline-block; padding: 0.1em 0.5em; border-radius: 4px; font-size: 0.75rem; font-weight: 600; }',
		'.cf-pass { background: rgba(16,185,129,0.14); color: #059669; }',
		'.cf-fail { background: rgba(220,38,38,0.14); color: #dc2626; }',
		'.cf-untested { background: rgba(125,125,125,0.14); opacity: 0.85; }',
		'.cf-meta { display: flex; flex-wrap: wrap; gap: 0.35rem 1.25rem; font-size: 0.8rem; opacity: 0.85; margin-top: 0.5rem; }',
		'.cf-preview { max-height: 220px; overflow: auto; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; font-size: 12px; background: rgba(125,125,125,0.08); padding: 0.6rem; border-radius: 4px; white-space: pre; }',
		'.cf-log { max-height: 220px; overflow: auto; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; font-size: 12px; white-space: pre-wrap; background: rgba(125,125,125,0.08); padding: 0.6rem; border-radius: 4px; }',
		'.cf-domains { min-height: 4.5em; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; }',
		'@media (max-width: 600px) {',
		'	.cf-grid { grid-template-columns: 1fr; }',
		'	.cf-table th, .cf-table td { padding: 0.35rem 0.4rem; }',
		'}'
	].join('\n');

	document.head.appendChild(E('style', { id: 'rust-dns-cloudflare-styles' }, css));
}

function fmtNumber(value, digits) {
	const n = Number(value);

	if (!isFinite(n))
		return '-';

	return n.toFixed(digits == null ? 2 : digits);
}

function isValidDomain(domain) {
	if (!domain || domain.length > 253)
		return false;
	if (!/^[\x21-\x7e]+$/.test(domain))
		return false;
	if (/[#\/\\:]/.test(domain))
		return false;
	if (domain.charAt(0) === '.' || domain.charAt(domain.length - 1) === '.')
		return false;

	const labels = domain.split('.');

	for (let i = 0; i < labels.length; i++) {
		const label = labels[i];

		if (label.length === 0 || label.length > 63)
			return false;
		if (label.charAt(0) === '-' || label.charAt(label.length - 1) === '-')
			return false;
		if (!/^[A-Za-z0-9_-]+$/.test(label))
			return false;
	}

	return true;
}

function stateLabel(value) {
	switch (value) {
	case 'running': return _('Running');
	case 'done': return _('Done');
	case 'cancelled': return _('Cancelled');
	case 'error': return _('Error');
	default: return _('Idle');
	}
}

function phaseLabel(value) {
	switch (value) {
	case 'latency': return _('Latency');
	case 'download': return _('Download');
	case 'verify': return _('Verify');
	default: return value || '';
	}
}

function isDryRunReport(report) {
	const bypass = (report && report.bypass) || {};
	return String(bypass.note || '').toLowerCase().indexOf('dry-run') !== -1;
}

function field(label, input, hint) {
	const children = [ E('label', {}, label), input ];

	if (hint)
		children.push(E('div', { class: 'cf-hint' }, hint));

	return E('div', { class: 'cf-field' }, children);
}

function render() {
	styleSheet();

	const inputs = {};
	const el = {
		inputs: inputs,
		sortHeaders: {},
		warnings: null,
		tableBody: null,
		tableHeadAll: null,
		reportMeta: null,
		selection: null,
		log: null,
		bar: null,
		stateText: null,
		phaseText: null,
		countText: null,
		message: null,
		start: null,
		cancel: null,
		applySelected: null,
		clear: null,
		root: null
	};
	state.el = el;

	function textInput(key, opts) {
		opts = opts || {};
		const input = E('input', {
			type: 'text',
			class: 'cbi-input-text',
			value: DEFAULTS[key] != null ? String(DEFAULTS[key]) : ''
		});

		if (opts.readonly)
			input.readOnly = true;
		if (opts.placeholder)
			input.placeholder = opts.placeholder;

		inputs[key] = input;
		return input;
	}

	function numberInput(key, min, max, step) {
		const input = E('input', {
			type: 'number',
			class: 'cbi-input-text',
			value: String(DEFAULTS[key]),
			min: String(min),
			max: String(max),
			step: step == null ? '1' : String(step)
		});

		inputs[key] = input;
		return input;
	}

	function selectInput(key, options) {
		const select = E('select', { class: 'cbi-input-select' },
			options.map(o => E('option', { value: o[0] }, o[1])));
		select.value = DEFAULTS[key];
		inputs[key] = select;
		return select;
	}

	function checkboxInput(key) {
		const input = E('input', { type: 'checkbox' });
		input.checked = DEFAULTS[key] === true;
		inputs[key] = input;
		return input;
	}

	const sourceSelect = selectInput('source', [
		[ 'cloudflare', _('Official Cloudflare ranges') ],
		[ 'file', _('Custom IP file') ],
		[ 'fixture', _('Built-in fixture') ]
	]);
	const familySelect = selectInput('family', [
		[ '4', _('IPv4') ],
		[ '6', _('IPv6') ]
	]);
	const bypassSelect = selectInput('bypass_mode', [
		[ 'none', _('None') ],
		[ 'mark', _('Mark') ],
		[ 'device', _('Device') ]
	]);

	const ipFileInput = textInput('ip_file', {
		placeholder: CF_DIR + '/ips.txt'
	});
	const outputInput = textInput('output', { readonly: true });
	const progressInput = textInput('progress', { readonly: true });
	const cancelInput = textInput('cancel', { readonly: true });

	const dryRunInput = checkboxInput('dry_run');
	const dryRunLabel = E('label', {
		style: 'display: inline-flex; align-items: center; gap: 0.4rem; font-weight: 600;'
	}, [ dryRunInput, _('Dry run (fixture, no network)') ]);

	const updateSourceState = () => {
		ipFileInput.disabled = sourceSelect.value !== 'file';
	};

	const updateBypassState = () => {
		const mode = bypassSelect.value;
		inputs.mark.disabled = mode !== 'mark';
		inputs.device.disabled = mode === 'none';
		inputs.source_ip.disabled = mode === 'none';
	};

	sourceSelect.addEventListener('change', updateSourceState);
	bypassSelect.addEventListener('change', updateBypassState);

	/* --- Test parameters ------------------------------------------------- */

	const params = E('div', { class: 'cf-grid' }, [
		field(_('IP source'), sourceSelect),
		field(_('IP file (under %s)').format(CF_DIR), ipFileInput,
			_('Used only with the custom IP file source. One IP or CIDR per line.')),
		field(_('Address family'), familySelect),
		field(_('Port'), numberInput('port', 1, 65535)),
		field(_('Threads'), numberInput('threads', 1, 512)),
		field(_('Ping count'), numberInput('ping', 1, 50)),
		field(_('Download tests'), numberInput('download_count', 0, 200)),
		field(_('Download time (seconds)'), numberInput('download_time', 0, 3600)),
		field(_('Download bytes'), numberInput('download_bytes', 0, 2000000000)),
		field(_('Download concurrency'), numberInput('download_concurrency', 1, 64)),
		field(_('Download URL'), textInput('url')),
		field(_('Maximum candidates'), numberInput('max_candidates', 1, 200000)),
		field(_('Maximum latency (ms)'), numberInput('max_latency', 0, 600000, '0.1')),
		field(_('Maximum loss (%)'), numberInput('max_loss', 0, 100, '0.1')),
		field(_('Minimum speed (Mbps)'), numberInput('min_speed', 0, 1000000, '0.1')),
		field(_('Dry run'), dryRunLabel,
			_('Uses the built-in fixture and skips all network and firewall changes.'))
	]);

	/* --- Bypass ---------------------------------------------------------- */

	const bypass = E('div', { class: 'cf-grid' }, [
		field(_('Bypass mode'), bypassSelect,
			_('mark uses SO_MARK with a temporary nft table; device binds to the WAN interface.')),
		field(_('Firewall mark'), textInput('mark')),
		field(_('Device (WAN interface)'), textInput('device', { placeholder: 'wan' })),
		field(_('Source IP (optional)'), textInput('source_ip', { placeholder: '203.0.113.10' }))
	]);

	/* --- Fixed output paths ---------------------------------------------- */

	const paths = E('div', { class: 'cf-grid' }, [
		field(_('Result JSON'), outputInput),
		field(_('Progress JSON'), progressInput),
		field(_('Cancel flag'), cancelInput)
	]);

	/* --- Actions and progress -------------------------------------------- */

	el.start = E('button', {
		class: 'btn cbi-button cbi-button-apply',
		click: startRun
	}, _('Start test'));

	el.cancel = E('button', {
		class: 'btn cbi-button cbi-button-reset',
		disabled: 'disabled',
		click: cancelRun
	}, _('Cancel test'));

	const refreshButton = E('button', {
		class: 'btn cbi-button cbi-button-neutral',
		click: () => {
			refreshResults(true);
			pollStatus();
		}
	}, _('Load last result'));

	el.bar = E('div', { style: 'width: 0;' });
	el.stateText = E('strong', {}, _('Idle'));
	el.phaseText = E('span', {}, '');
	el.countText = E('span', {}, '');
	el.message = E('span', {}, '');

	const progressBox = E('div', {}, [
		E('div', { class: 'cf-progress' }, el.bar),
		E('div', { class: 'cf-status' }, [
			el.stateText,
			el.phaseText,
			el.countText,
			el.message
		])
	]);

	/* --- Warnings -------------------------------------------------------- */

	el.warnings = E('div', {});

	/* --- Results --------------------------------------------------------- */

	const domainsInput = E('textarea', {
		class: 'cf-domains',
		rows: '3',
		placeholder: 'cloudflare.com'
	});
	domainsInput.value = 'cloudflare.com';
	inputs.domains = domainsInput;

	const filterInput = E('input', {
		type: 'text',
		class: 'cbi-input-text',
		placeholder: _('Filter by IP...')
	});

	filterInput.addEventListener('input', () => {
		state.filter = filterInput.value.trim().toLowerCase();
		renderResults();
	});

	const passedSelect = E('select', { class: 'cbi-input-select' }, [
		E('option', { value: 'all' }, _('All results')),
		E('option', { value: 'passed' }, _('Passed only')),
		E('option', { value: 'failed' }, _('Failed only'))
	]);

	passedSelect.addEventListener('change', () => {
		state.passedFilter = passedSelect.value;
		renderResults();
	});

	el.selection = E('span', { class: 'cf-hint' }, _('Selected: %d').format(0));

	el.applySelected = E('button', {
		class: 'btn cbi-button cbi-button-action',
		click: () => requestApply(selectedIps())
	}, _('Apply selected IPs to hosts'));

	el.clear = E('button', {
		class: 'btn cbi-button cbi-button-remove',
		click: requestClear
	}, _('Clear results'));

	el.tableHeadAll = E('input', { type: 'checkbox' });
	el.tableHeadAll.addEventListener('change', () => {
		const rows = filteredRows().filter(row => row.passed === true);
		const want = el.tableHeadAll.checked;

		rows.forEach(row => {
			if (want)
				state.selected[row.ip] = true;
			else
				delete state.selected[row.ip];
		});

		renderResults();
	});

	function sortHeader(key, label) {
		const th = E('th', { class: 'sortable', title: _('Click to sort') }, label);

		th.addEventListener('click', () => {
			if (state.sortKey === key) {
				state.sortDir = (state.sortDir === 'asc') ? 'desc' : 'asc';
			}
			else {
				state.sortKey = key;
				state.sortDir = (key === 'ip' || key === 'latency_min_ms' ||
					key === 'latency_avg_ms' || key === 'loss_pct') ? 'asc' : 'desc';
			}

			renderResults();
		});

		el.sortHeaders[key] = { node: th, label: label };
		return th;
	}

	el.tableBody = E('tbody');

	const table = E('div', { class: 'cf-table-wrap' }, [
		E('table', { class: 'cf-table' }, [
			E('thead', {}, E('tr', {}, [
				E('th', { style: 'width: 2.2em;' }, el.tableHeadAll),
				sortHeader('ip', _('IP address')),
				sortHeader('family', _('Family')),
				sortHeader('latency_min_ms', _('Min latency (ms)')),
				sortHeader('latency_avg_ms', _('Avg latency (ms)')),
				sortHeader('loss_pct', _('Loss (%)')),
				sortHeader('speed_mbps', _('Speed (Mbps)')),
				sortHeader('passed', _('Status')),
				E('th', {}, _('Actions'))
			])),
			el.tableBody
		])
	]);

	el.reportMeta = E('div', { class: 'cf-meta' }, _('No results yet.'));

	el.log = E('pre', { class: 'cf-log' }, '');

	const logDetails = E('details', { style: 'margin-top: 0.75rem;' }, [
		E('summary', {}, _('Run log')),
		el.log
	]);

	/* --- Assemble -------------------------------------------------------- */

	const root = E('div', { class: 'cbi-map' }, [
		E('h2', { name: 'content' }, _('Cloudflare IP Optimizer')),
		E('div', { class: 'cbi-section' }, [
			E('p', { class: 'cf-hint' },
				_('Select fast Cloudflare IPs and write them to the rust-dns hosts rules. This does not modify the system /etc/hosts or the OpenClash configuration.')),
			el.warnings,
			E('div', { class: 'cf-section' }, [
				E('h3', {}, _('Test Parameters')),
				params
			]),
			E('div', { class: 'cf-section' }, [
				E('h3', {}, _('OpenClash Bypass')),
				bypass,
				E('p', { class: 'cf-hint' },
					_('Bypass verification is best-effort. If the result reports an unverified bypass, Cloudflare traffic may still go through the proxy.'))
			]),
			E('div', { class: 'cf-section' }, [
				E('h3', {}, _('Output Paths (fixed)')),
				paths
			]),
			E('div', { class: 'cf-section' }, [
				E('h3', {}, _('Run Status')),
				E('div', { class: 'cf-actions' }, [ el.start, el.cancel, refreshButton ]),
				progressBox,
				logDetails
			]),
			E('div', { class: 'cf-section' }, [
				E('h3', {}, _('Target Domains')),
				field(_('Domains'), domainsInput,
					_('One domain per line or comma separated. Default: cloudflare.com.')),
				E('div', { class: 'cf-toolbar' }, [
					filterInput,
					passedSelect,
					el.selection,
					el.applySelected,
					el.clear
				]),
				table,
				el.reportMeta
			])
		])
	]);

	el.root = root;

	updateSourceState();
	updateBypassState();
	updateButtons();
	updateWarnings();

	/* Initial data load; failures degrade to rendered messages. */
	pollStatus();
	refreshResults(false);

	if (!state.pollRegistered) {
		state.pollRegistered = true;
		poll.add(pollStatus, 1);
	}

	return root;
}

function val(key) {
	const input = state.el && state.el.inputs[key];
	return input ? input.value : '';
}

function getConfig() {
	return {
		source: val('source'),
		ip_file: val('ip_file'),
		family: val('family'),
		port: val('port'),
		threads: val('threads'),
		ping: val('ping'),
		download_count: val('download_count'),
		download_time: val('download_time'),
		download_bytes: val('download_bytes'),
		download_concurrency: val('download_concurrency'),
		url: val('url'),
		max_candidates: val('max_candidates'),
		max_latency: val('max_latency'),
		max_loss: val('max_loss'),
		min_speed: val('min_speed'),
		bypass_mode: val('bypass_mode'),
		mark: val('mark'),
		device: val('device'),
		source_ip: val('source_ip'),
		dry_run: !!(state.el && state.el.inputs.dry_run.checked)
	};
}

function updateButtons() {
	const el = state.el;

	if (!el)
		return;

	const status = state.status || {};
	const running = status.running === true || status.state === 'running';
	const available = status.available !== false;

	el.start.disabled = state.busy || running || !available;
	el.cancel.disabled = state.busy || !running;
	el.applySelected.disabled = state.busy || selectedIps().length === 0;
	el.clear.disabled = state.busy;
}

function updateWarnings() {
	const el = state.el;

	if (!el)
		return;

	const items = [];

	if (state.status == null) {
		items.push({ level: 'info', text: _('Checking optimizer status...') });
	}
	else {
		const status = state.status;

		if (status.available === false)
			items.push({ level: 'error', text: _('The Rust optimizer binary or its cf-optimize subcommand is missing. Install a rust-dns build that provides it.') });
		if (status.error)
			items.push({ level: 'error', text: _('Status API error: %s').format(status.error) });
		if (status.progress && status.progress.state === 'error' && status.progress.message)
			items.push({ level: 'error', text: _('The run failed: %s').format(status.progress.message) });
	}

	if (state.report) {
		if (isDryRunReport(state.report))
			items.push({ level: 'warning', text: _('Dry-run result: these values come from the built-in fixture and are not real measurements.') });

		const bypass = state.report.bypass || {};

		if (bypass.mode && bypass.mode !== 'none' && bypass.verified !== true)
			items.push({ level: 'warning', text: _('Bypass was not verified. Results may have traversed the proxy. Try device mode or check the OpenClash configuration.') });
	}

	el.warnings.innerHTML = '';

	items.forEach(item => {
		el.warnings.appendChild(E('div', { class: 'cf-' + (item.level === 'error' ? 'error' : (item.level === 'info' ? 'info' : 'warning')) }, item.text));
	});
}

function applyStatus(res) {
	res = res || {};
	state.status = res;

	const el = state.el;

	if (!el || !document.body.contains(el.root)) {
		updateButtons();
		return;
	}

	const progress = res.progress || null;
	const runState = res.state || (res.running ? 'running' : 'idle');
	const phase = progress && progress.phase ? progress.phase : '';
	const tested = progress ? (Number(progress.tested) || 0) : 0;
	const total = progress ? (Number(progress.total) || 0) : 0;
	let percent = 0;

	if (total > 0)
		percent = Math.min(100, Math.round(tested / total * 100));
	else if (runState === 'done')
		percent = 100;

	el.stateText.textContent = stateLabel(runState);
	el.phaseText.textContent = phase ? phaseLabel(phase) : '';
	el.countText.textContent = total > 0 ? _('%d of %d').format(tested, total) : '';
	el.message.textContent = (progress && progress.message) ? progress.message : '';
	el.bar.style.width = percent + '%';
	el.bar.style.background = (runState === 'error') ? '#dc2626' :
		(runState === 'cancelled') ? '#d97706' :
		(runState === 'done') ? '#059669' : '#3b82f6';

	const tail = res.log_tail || '';

	if (el.log.textContent !== tail)
		el.log.textContent = tail;

	updateButtons();
	updateWarnings();

	const previous = state.lastState;
	state.lastState = runState;

	if (previous === 'running' && runState !== 'running' && runState !== 'idle') {
		state.busy = false;

		if (runState === 'done') {
			note('info', _('The run has finished. Results were reloaded.'));
			refreshResults(false);
		}
		else if (runState === 'cancelled') {
			note('warning', _('The run was cancelled.'));
			refreshResults(false);
		}
		else if (runState === 'error') {
			note('error', _('The run failed: %s').format((progress && progress.message) || _('Unknown error')));
			refreshResults(false);
		}
	}

	if (state.busy && runState === 'running')
		state.busy = false;

	updateButtons();
}

function pollStatus() {
	return safeCall(callCfStatus, {
		available: false,
		state: 'idle',
		error: _('Status API is unavailable.')
	}).then(res => {
		if (!state.el || !document.body.contains(state.el.root))
			return;

		applyStatus(res);
	});
}

function refreshResults(showNotification) {
	if (state.refreshInFlight)
		return Promise.resolve();

	state.refreshInFlight = true;

	return safeCall(callCfResults, {
		success: false,
		error: _('Results API is unavailable.')
	}).then(res => {
		state.refreshInFlight = false;
		res = res || {};

		if (res.success && res.report) {
			state.report = res.report;

			/* Drop selections for IPs that are no longer part of the result. */
			const seen = {};
			const results = res.report.results || [];

			results.forEach(row => {
				if (state.selected[row.ip])
					seen[row.ip] = true;
			});
			state.selected = seen;

			renderResults();
			renderReportMeta(res.report);
			updateWarnings();

			if (showNotification)
				note('info', _('Last result loaded.'));
		}
		else {
			state.report = null;
			state.selected = {};
			renderResults();
			renderReportMeta(null);
			updateWarnings();

			if (showNotification && !res.missing && res.error)
				note('error', _('Could not load results: %s').format(res.error));
		}
	}).catch(e => {
		state.refreshInFlight = false;
		note('error', _('Could not load results: %s').format((e && e.message) || String(e)));
	});
}

function startRun() {
	if (state.busy)
		return;

	const config = getConfig();

	if (config.source === 'file' && config.ip_file === '') {
		note('error', _('IP file path is empty.'));
		return;
	}

	state.busy = true;
	updateButtons();

	safeCall(() => callCfStart(config), {
		success: false,
		error: _('Optimizer API is unavailable.')
	}).then(res => {
		state.busy = false;
		res = res || {};

		if (res.success) {
			/* A new run starts clean; do not let stale results be applied. */
			state.report = null;
			state.selected = {};
			state.lastState = 'running';
			state.status = state.status || {};
			state.status.available = true;
			state.status.running = true;
			state.status.state = 'running';
			renderResults();
			renderReportMeta(null);
			updateButtons();
			note('info', _('Started run %s.').format(res.run_id || ''));
			pollStatus();
		}
		else {
			if (res.error && /does not provide|binary not found/i.test(res.error)) {
				state.status = state.status || {};
				state.status.available = false;
				updateWarnings();
			}

			note('error', res.error || _('Failed to start the optimizer.'));
			updateButtons();
		}
	});
}

function cancelRun() {
	if (state.busy)
		return;

	state.busy = true;
	updateButtons();

	safeCall(callCfCancel, {
		success: false,
		error: _('Cancel API is unavailable.')
	}).then(res => {
		state.busy = false;
		res = res || {};

		if (res.success)
			note('info', _('Cancel requested.'));
		else
			note('error', res.error || _('Failed to cancel the run.'));

		pollStatus();
	});
}

function selectedIps() {
	return Object.keys(state.selected).filter(ip => state.selected[ip] === true);
}

function filteredRows() {
	const results = (state.report && state.report.results) || [];
	const filter = state.filter;
	const passed = state.passedFilter;

	return results.filter(row => {
		if (filter && String(row.ip || '').toLowerCase().indexOf(filter) === -1)
			return false;
		if (passed === 'passed' && !row.passed)
			return false;
		// "Failed" means the row reached the speed test and missed a
		// threshold. Rows outside the download winners were never tested.
		if (passed === 'failed' && (row.passed || row.download_tested !== true))
			return false;
		return true;
	});
}

function sortedRows(rows) {
	const key = state.sortKey;
	const factor = (state.sortDir === 'asc') ? 1 : -1;

	return rows.slice().sort((a, b) => {
		let av = a[key];
		let bv = b[key];

		if (key === 'passed') {
			av = av ? 1 : 0;
			bv = bv ? 1 : 0;
		}
		else if (key === 'ip' || key === 'family') {
			av = String(av == null ? '' : av);
			bv = String(bv == null ? '' : bv);

			if (av === bv)
				return 0;
			return (av < bv ? -1 : 1) * factor;
		}
		else {
			av = Number(av) || 0;
			bv = Number(bv) || 0;
		}

		return (av - bv) * factor;
	});
}

function renderResults() {
	const el = state.el;

	if (!el || !el.tableBody)
		return;

	const rows = sortedRows(filteredRows());
	const visibleIps = {};

	el.tableBody.innerHTML = '';

	if (rows.length === 0) {
		el.tableBody.appendChild(E('tr', {}, [
			E('td', { class: 'cf-empty', colspan: '9' },
				state.report ? _('No results match the current filter.') : _('No results yet. Run a test or load the last result.'))
		]));
	}
	else {
		rows.forEach(row => {
			const applicable = row.passed === true;
			const status = applicable
				? { cls: 'cf-pass', label: _('Passed'), title: '' }
				: (row.download_tested === true
					? { cls: 'cf-fail', label: _('Failed'), title: '' }
					: { cls: 'cf-untested', label: _('Not tested'),
						title: _('Only the fastest latency results receive a download test.') });

			if (applicable)
				visibleIps[row.ip] = true;

			const checkbox = E('input', { type: 'checkbox' });
			checkbox.disabled = !applicable;
			checkbox.checked = applicable && state.selected[row.ip] === true;
			checkbox.addEventListener('change', () => {
				if (checkbox.checked)
					state.selected[row.ip] = true;
				else
					delete state.selected[row.ip];

				updateSelectionSummary();
				updateButtons();
			});

			const writeAttrs = {
				class: 'btn cbi-button cbi-button-action',
				click: () => requestApply([ row.ip ])
			};

			if (!applicable) {
				writeAttrs.disabled = 'disabled';
				writeAttrs.title = _('Only results that passed the thresholds can be written.');
			}

			const writeButton = E('button', writeAttrs, _('Write to hosts'));

			el.tableBody.appendChild(E('tr', {}, [
				E('td', {}, checkbox),
				E('td', { class: 'cf-mono' }, row.ip || '-'),
				E('td', {}, String(row.family == null ? '-' : row.family)),
				E('td', { class: 'cf-mono' }, fmtNumber(row.latency_min_ms)),
				E('td', { class: 'cf-mono' }, fmtNumber(row.latency_avg_ms)),
				E('td', { class: 'cf-mono' }, fmtNumber(row.loss_pct)),
				E('td', { class: 'cf-mono' }, fmtNumber(row.speed_mbps)),
				E('td', {}, E('span', { class: 'cf-badge ' + status.cls, title: status.title },
					status.label)),
				E('td', {}, writeButton)
			]));
		});
	}

	/* Sort indicators. */
	Object.keys(el.sortHeaders).forEach(key => {
		const header = el.sortHeaders[key];
		header.node.textContent = header.label +
			((state.sortKey === key) ? (state.sortDir === 'asc' ? ' ▲' : ' ▼') : '');
	});

	/* Select-all reflects the visible rows. */
	const visible = Object.keys(visibleIps);
	let allSelected = visible.length > 0;

	visible.forEach(ip => {
		if (state.selected[ip] !== true)
			allSelected = false;
	});

	el.tableHeadAll.checked = allSelected;
	el.tableHeadAll.disabled = visible.length === 0;
	updateSelectionSummary();
	updateButtons();
}

function updateSelectionSummary() {
	const el = state.el;

	if (!el || !el.selection)
		return;

	const count = selectedIps().length;
	el.selection.textContent = _('Selected: %d').format(count);

	if (el.applySelected)
		el.applySelected.disabled = state.busy || count === 0;
}

function renderReportMeta(report) {
	const el = state.el;

	if (!el || !el.reportMeta)
		return;

	el.reportMeta.innerHTML = '';

	if (!report) {
		el.reportMeta.textContent = _('No results yet. Run a test or load the last result.');
		return;
	}

	const bypass = report.bypass || {};
	const results = report.results || [];
	const tested = results.filter(row => row.download_tested === true).length;
	const passedCount = results.filter(row => row.passed === true).length;
	const parts = [
		_('Run ID') + ': ' + (report.run_id || '-'),
		_('Started') + ': ' + (report.started_at || '-'),
		_('Finished') + ': ' + (report.finished_at || '-'),
		_('Results') + ': ' + results.length,
		_('Speed-tested') + ': ' + tested,
		_('Passed') + ': ' + passedCount,
		_('Bypass') + ': ' + (bypass.mode || '-') +
			(bypass.mode && bypass.mode !== 'none'
				? ' (' + (bypass.verified ? _('Verified') : _('Unverified')) + ')'
				: '')
	];

	if (bypass.egress_ip)
		parts.push(_('Egress IP') + ': ' + bypass.egress_ip);

	parts.forEach(text => el.reportMeta.appendChild(E('span', {}, text)));
}

function parseDomains() {
	const input = state.el && state.el.inputs.domains;
	const raw = input ? String(input.value || '') : '';
	const parts = raw.split(/[\s,]+/);
	const seen = {};
	const domains = [];

	for (let i = 0; i < parts.length; i++) {
		const candidate = parts[i].trim();

		if (candidate === '')
			continue;

		const domain = candidate.toLowerCase();

		if (!isValidDomain(domain))
			return { error: _('Invalid domain: %s').format(candidate) };

		if (!seen[domain]) {
			seen[domain] = true;
			domains.push(domain);
		}
	}

	if (domains.length === 0)
		return { error: _('No target domain configured.') };

	return { domains: domains };
}

function requestApply(ips) {
	if (state.busy) {
		note('warning', _('Wait for the current operation to finish.'));
		return;
	}

	if (!ips || ips.length === 0) {
		note('warning', _('Select at least one IP.'));
		return;
	}

	if (state.status && (state.status.running === true || state.status.state === 'running')) {
		note('warning', _('Wait for the current run to finish before writing hosts.'));
		return;
	}

	const parsed = parseDomains();

	if (parsed.error) {
		note('error', parsed.error);
		return;
	}

	const uniqueIps = [];

	ips.forEach(ip => {
		if (ip && uniqueIps.indexOf(ip) === -1)
			uniqueIps.push(ip);
	});

	const passed = {};
	((state.report && state.report.results) || []).forEach(row => {
		if (row.passed === true)
			passed[row.ip] = true;
	});

	const applicable = uniqueIps.filter(ip => passed[ip] === true);

	if (applicable.length === 0) {
		note('warning', _('Only results that passed the thresholds can be written.'));
		return;
	}

	if (applicable.length !== uniqueIps.length)
		note('warning', _('Some selected IPs did not pass the thresholds and will be ignored.'));

	const lines = [];

	parsed.domains.forEach(domain => {
		applicable.forEach(ip => lines.push(domain + ' ' + ip));
	});
	lines.sort();

	const children = [
		E('p', {}, _('The following entries will be written to %s. Existing content outside the managed block is preserved.').format(HOSTS_FILE)),
		E('pre', { class: 'cf-preview' }, lines.join('\n'))
	];

	if (applicable.length > 1)
		children.push(E('p', { class: 'cf-hint' },
			_('Multiple IPs per domain produce multiple hosts lines; the hosts plugin may return more than one address.')));

	children.push(E('div', { class: 'right', style: 'margin-top: 1rem;' }, [
		E('button', { class: 'btn cbi-button cbi-button-neutral', click: ui.hideModal }, _('Cancel')),
		' ',
		E('button', {
			class: 'btn cbi-button cbi-button-apply',
			click: () => {
				ui.hideModal();
				applyHosts(parsed.domains, applicable);
			}
		}, _('Write to hosts'))
	]));

	ui.showModal(_('Confirm writing hosts'), children);
}

function applyHosts(domains, ips) {
	state.busy = true;
	updateButtons();

	safeCall(() => callCfApplyHosts(domains, ips), {
		success: false,
		error: _('Hosts API is unavailable.')
	}).then(res => {
		state.busy = false;
		res = res || {};

		if (res.success) {
			note('info', _('%d entries written to hosts.').format(res.written || 0));
		}
		else {
			note('error', res.error || _('Failed to write hosts.'));
		}

		updateButtons();
	});
}

function requestClear() {
	if (state.busy || (state.status && (state.status.running === true || state.status.state === 'running'))) {
		note('warning', _('Wait for the current run to finish before clearing results.'));
		return;
	}

	const children = [
		E('p', {}, _('This removes last.json, result.csv, progress.json, the run log and the managed block in %s. It does not touch the system /etc/hosts or the OpenClash configuration.').format(HOSTS_FILE)),
		E('div', { class: 'right', style: 'margin-top: 1rem;' }, [
			E('button', { class: 'btn cbi-button cbi-button-neutral', click: ui.hideModal }, _('Cancel')),
			' ',
			E('button', {
				class: 'btn cbi-button cbi-button-remove',
				click: () => {
					ui.hideModal();
					clearResults();
				}
			}, _('Clear'))
		])
	];

	ui.showModal(_('Clear results and managed hosts block?'), children);
}

function clearResults() {
	state.busy = true;
	updateButtons();

	safeCall(callCfClear, {
		success: false,
		error: _('Clear API is unavailable.')
	}).then(res => {
		state.busy = false;
		res = res || {};

		if (res.success) {
			state.report = null;
			state.selected = {};
			renderResults();
			renderReportMeta(null);
			updateWarnings();
			note('info', _('Cleared results and the managed hosts block.'));
		}
		else {
			note('error', res.error || _('Failed to clear results.'));
		}

		pollStatus();
	});
}

return view.extend({
	render: render,
	handleSave: null,
	handleSaveApply: null,
	handleReset: null
});
