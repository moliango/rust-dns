'use strict';
'require rpc';
'require ui';
'require view';

/*
 * SPDX-License-Identifier: GPL-3.0-only
 *
 * System Upgrade page.
 *
 * A thin frontend for the upgrade_check / upgrade_start / upgrade_status
 * methods of luci.rust_dns. The upgrade tool it drives is shipped by the
 * rust-dns service package (/usr/share/rust-dns/rust-dns.uc); this view only
 * displays its results and polls the log it writes. The download source and
 * every file path are fixed in the backend.
 */

const callGetVersion = rpc.declare({
	object: 'luci.rust_dns',
	method: 'get_version',
	expect: { '': {} }
});

const callUpgradeCheck = rpc.declare({
	object: 'luci.rust_dns',
	method: 'upgrade_check',
	expect: { '': {} }
});

const callUpgradeStart = rpc.declare({
	object: 'luci.rust_dns',
	method: 'upgrade_start',
	expect: { '': {} }
});

const callUpgradeStatus = rpc.declare({
	object: 'luci.rust_dns',
	method: 'upgrade_status',
	expect: { '': {} }
});

const UPGRADE_EXIT_RE = /UPGRADE_EXITED[ \t]+(\d+)/;
const POLL_INTERVAL_MS = 1500;
const UPGRADE_TIMEOUT_MS = 10 * 60 * 1000;
const START_GRACE_MS = 60 * 1000;

const state = {
	el: null,
	status: null,
	installed: null,
	busy: false,
	inFlight: false,
	polling: null,
	startedAt: 0,
	visibilityRegistered: false
};

function errorText(e) {
	return (e && e.message) ? e.message : String(e);
}

function str(value) {
	return (typeof value === 'string') ? value : '';
}

function safeCall(fn, fallback) {
	try {
		return L.resolveDefault(fn(), fallback).catch(e => ({
			error: errorText(e)
		}));
	}
	catch (e) {
		return Promise.resolve({ error: errorText(e) });
	}
}

function hasCheckData(res) {
	if (!res)
		return false;

	return res.current != null || res.latest != null ||
		res.update_available != null || res.tag != null;
}

function packageNames(res) {
	const list = Array.isArray(res && res.packages) ? res.packages : [];
	const names = [];

	list.forEach(pkg => {
		if (pkg && typeof pkg.name === 'string')
			names.push(pkg.name);
	});

	return names.join(', ');
}

function setNote(message, kind) {
	const el = state.el;

	if (!el)
		return;

	el.note.className = (kind === 'spinning') ? 'spinning' : '';
	el.note.style.color =
		(kind === 'success') ? '#19be6b' :
		(kind === 'error') ? '#ed4014' :
		(kind === 'warning') ? '#ff9900' : '';
	el.note.textContent = message || '';
}

function isRunning() {
	return !!(state.status &&
		(state.status.running === true || state.status.state === 'running'));
}

function updateButtons() {
	const el = state.el;

	if (!el)
		return;

	const running = state.busy || isRunning();
	const available = !!(state.status && state.status.update_available === true);

	el.check.disabled = state.busy || isRunning();
	el.upgrade.disabled = running || !available;
}

function applyStatus(res) {
	const el = state.el;

	if (!el)
		return;

	res = res || {};
	state.status = res;

	const current = str(res.current) || state.installed || _('Unknown');
	const latest = str(res.latest) || (hasCheckData(res) ? _('Unknown') : _('Not checked yet'));

	el.current.textContent = current;
	el.latest.textContent = latest;
	el.arch.textContent = str(res.arch) || _('Unknown');
	el.openwrt.textContent = str(res.openwrt) || _('Unknown');
	el.packageManager.textContent = str(res.package_manager) || _('Unknown');
	el.packages.textContent = packageNames(res) || _('Unknown');

	if (!hasCheckData(res))
		el.available.textContent = _('Not checked yet');
	else if (res.update_available === true)
		el.available.textContent = _('Update available');
	else
		el.available.textContent = _('Already up to date');

	const tail = str(res.log_tail);

	if (el.log.value !== tail) {
		el.log.value = tail;
		el.log.scrollTop = el.log.scrollHeight;
	}

	updateButtons();
}

function stopPolling() {
	if (state.polling !== null) {
		window.clearInterval(state.polling);
		state.polling = null;
	}
}

function ensurePolling() {
	if (state.polling !== null)
		return;

	if (!state.startedAt)
		state.startedAt = Date.now();

	state.polling = window.setInterval(() => {
		const el = state.el;

		if (!el || !document.body.contains(el.root)) {
			stopPolling();
			return;
		}

		/* Do not keep the RPC loop busy for a background tab. */
		if (document.visibilityState === 'hidden')
			return;

		refreshStatus();
	}, POLL_INTERVAL_MS);

	if (!state.visibilityRegistered) {
		state.visibilityRegistered = true;
		document.addEventListener('visibilitychange', () => {
			if (document.visibilityState === 'visible' && state.polling !== null)
				refreshStatus();
		});
	}
}

function timedOut() {
	return !!(state.startedAt && Date.now() - state.startedAt > UPGRADE_TIMEOUT_MS);
}

function finishUpgrade(res, exitCode) {
	stopPolling();
	state.startedAt = 0;

	if (exitCode === 0 || res.state === 'done') {
		// The installed version is now the target offered by the last check.
		// Reflect that locally and require a fresh check before offering another
		// upgrade, rather than trusting the now-stale check file.
		if (state.status) {
			if (str(state.status.latest) !== '')
				state.status.current = state.status.latest;
			state.status.update_available = false;
			state.status.running = false;
		}

		if (state.el) {
			if (state.status && str(state.status.current) !== '')
				state.el.current.textContent = state.status.current;
			state.el.available.textContent = _('Already up to date');
		}

		setNote(_('Upgrade completed successfully. LuCI may reload because rpcd/uhttpd was restarted.'), 'success');
	}
	else if (exitCode !== null) {
		const suffix = res.error ? ' ' + res.error : '';
		setNote(_('Upgrade failed (exit code %d).').format(exitCode) + suffix, 'error');
	}
	else if (res.state === 'error') {
		setNote(res.error || _('Upgrade failed.'), 'error');
	}
	else {
		setNote(_('Upgrade stopped without reporting an exit code. Check the log.'), 'warning');
	}

	updateButtons();
}

function handleStatus(res) {
	const tail = str(res.log_tail);
	const exitMatch = tail.match(UPGRADE_EXIT_RE);
	const exitCode = exitMatch ? parseInt(exitMatch[1], 10) : null;

	if (res.state === 'running') {
		ensurePolling();

		if (state.el && state.el.logDetails)
			state.el.logDetails.open = true;

		setNote(_('Upgrade is running...'), 'spinning');

		if (timedOut()) {
			stopPolling();
			state.startedAt = 0;
			setNote(_('Upgrade is still running. Check the log for progress.'), 'warning');
			updateButtons();
		}

		return;
	}

	if (state.polling === null) {
		if (res.state === 'error' && res.error)
			setNote(res.error, 'error');

		return;
	}

	// A freshly launched upgrade can briefly look idle before the detached
	// shell truncates the log and the tool writes its lock.
	if (res.state === 'idle' && exitCode === null &&
	    state.startedAt && Date.now() - state.startedAt < START_GRACE_MS) {
		setNote(_('Starting upgrade...'), 'spinning');
		return;
	}

	finishUpgrade(res, exitCode);
}

function refreshStatus() {
	if (state.inFlight)
		return Promise.resolve();

	state.inFlight = true;

	return safeCall(callUpgradeStatus, {
		error: _('Upgrade status API is unavailable.')
	}).then(res => {
		state.inFlight = false;

		if (!state.el || !document.body.contains(state.el.root))
			return;

		// An RPC failure during the upgrade is expected while rpcd restarts.
		// Keep polling until the timeout instead of declaring failure.
		if (res && res.state == null && res.log_tail == null && res.error) {
			if (state.polling !== null) {
				setNote(_('Upgrade status is unavailable. Waiting for rpcd to come back...'), 'warning');

				if (timedOut()) {
					stopPolling();
					state.startedAt = 0;
					setNote(_('Upgrade is still running. Check the log for progress.'), 'warning');
				}

				updateButtons();
				return;
			}

			setNote(_('Failed to read upgrade status: %s').format(res.error), 'error');
			updateButtons();
			return;
		}

		applyStatus(res);
		handleStatus(res);
	}).catch(e => {
		state.inFlight = false;

		if (!state.el || !document.body.contains(state.el.root))
			return;

		if (state.polling !== null)
			setNote(_('Upgrade status is unavailable. Waiting for rpcd to come back...'), 'warning');
		else
			setNote(_('Failed to read upgrade status: %s').format(errorText(e)), 'error');

		updateButtons();
	});
}

function handleCheck() {
	if (state.busy || isRunning())
		return;

	state.busy = true;
	setNote(_('Checking for updates...'), 'spinning');
	updateButtons();

	safeCall(callUpgradeCheck, {
		success: false,
		error: _('Upgrade check API is unavailable.')
	}).then(res => {
		res = res || {};

		return refreshStatus().then(() => {
			state.busy = false;

			if (res.success) {
				setNote(res.update_available === true
					? _('Update available.')
					: _('Already up to date.'),
					(res.update_available === true) ? 'success' : 'info');
			}
			else {
				setNote(res.error || _('Failed to check for updates.'), 'error');
			}

			updateButtons();
		});
	}).catch(e => {
		state.busy = false;
		setNote(_('Failed to check for updates: %s').format(errorText(e)), 'error');
		updateButtons();
	});
}

function startUpgrade() {
	ui.hideModal();

	if (state.busy)
		return;

	state.busy = true;
	state.startedAt = 0;
	setNote(_('Starting upgrade...'), 'spinning');
	updateButtons();

	safeCall(callUpgradeStart, {
		success: false,
		error: _('Upgrade API is unavailable.')
	}).then(res => {
		state.busy = false;
		res = res || {};

		if (!res.success) {
			setNote(res.error || _('Failed to start the upgrade.'), 'error');
			updateButtons();
			return;
		}

		state.startedAt = Date.now();

		if (state.el && state.el.logDetails)
			state.el.logDetails.open = true;

		ensurePolling();
		refreshStatus();
	}).catch(e => {
		state.busy = false;
		setNote(_('Failed to start the upgrade: %s').format(errorText(e)), 'error');
		updateButtons();
	});
}

function requestUpgrade() {
	if (state.busy || isRunning())
		return;

	if (!state.status || state.status.update_available !== true)
		return;

	ui.showModal(_('Upgrade rust-dns?'), [
		E('p', {}, _('The router will download and install the latest rust-dns packages from GitHub, then restart the service. Do not power off the router during this process.')),
		E('p', {}, _('LuCI may reload because rpcd or uhttpd is restarted after the upgrade.')),
		E('div', { class: 'right', style: 'margin-top: 1rem;' }, [
			E('button', {
				class: 'btn cbi-button cbi-button-neutral',
				click: ui.hideModal
			}, _('Cancel')),
			' ',
			E('button', {
				class: 'btn cbi-button cbi-button-apply',
				click: startUpgrade
			}, _('Upgrade'))
		])
	]);
}

function render() {
	const el = {};

	el.current = E('span', {}, _('Unknown'));
	el.latest = E('span', {}, _('Not checked yet'));
	el.available = E('strong', {}, _('Not checked yet'));
	el.arch = E('span', {}, _('Unknown'));
	el.openwrt = E('span', {}, _('Unknown'));
	el.packageManager = E('span', {}, _('Unknown'));
	el.packages = E('span', {}, _('Unknown'));

	el.check = E('button', {
		class: 'btn cbi-button cbi-button-action',
		click: handleCheck
	}, _('Check for updates'));

	el.upgrade = E('button', {
		class: 'btn cbi-button cbi-button-apply',
		click: requestUpgrade
	}, _('Upgrade now'));

	el.note = E('p', {});

	el.log = E('textarea', {
		class: 'cbi-input-textarea',
		readonly: 'readonly',
		rows: '14',
		placeholder: _('No upgrade log available.'),
		style: 'width: 100%; box-sizing: border-box; height: 280px; ' +
			'font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; ' +
			'font-size: 12px;'
	});

	el.logDetails = E('details', { style: 'margin-top: 1rem;' }, [
		E('summary', {}, _('Upgrade log')),
		el.log
	]);

	const rows = [
		[ _('Installed version'), el.current ],
		[ _('Latest version'), el.latest ],
		[ _('Update status'), el.available ],
		[ _('Architecture'), el.arch ],
		[ _('OpenWrt version'), el.openwrt ],
		[ _('Package manager'), el.packageManager ],
		[ _('Packages'), el.packages ]
	];

	el.root = E('div', { class: 'cbi-map' }, [
		E('h2', { name: 'content' }, _('System Upgrade')),
		E('div', { class: 'cbi-section' }, [
			E('p', {}, _('Check for a newer rust-dns release on GitHub. Only packages matching this router\'s OpenWrt branch and architecture are offered, and every package is verified against SHA256SUMS-packages before installation.')),
			E('table', { class: 'table' }, rows.map(row => E('tr', {}, [
				E('th', { style: 'width: 30%; text-align: left;' }, row[0]),
				E('td', {}, row[1])
			]))),
			E('div', { style: 'margin-top: 1rem;' }, [ el.check, ' ', el.upgrade ]),
			el.note,
			el.logDetails
		])
	]);

	state.el = el;
	state.status = null;
	state.installed = null;
	state.busy = false;
	state.inFlight = false;
	state.startedAt = 0;
	stopPolling();

	updateButtons();

	safeCall(callGetVersion, {}).then(res => {
		if (!state.el || !document.body.contains(state.el.root))
			return;

		if (res && typeof res.version === 'string' && res.version !== '') {
			state.installed = res.version;

			if (!state.status || state.status.current == null)
				el.current.textContent = res.version;
		}
	});

	refreshStatus();

	return el.root;
}

return view.extend({
	render: render,
	handleSave: null,
	handleSaveApply: null,
	handleReset: null
});
