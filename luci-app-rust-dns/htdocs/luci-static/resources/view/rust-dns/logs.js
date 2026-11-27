'use strict';
'require poll';
'require rpc';
'require ui';
'require view';

/*
 * SPDX-License-Identifier: GPL-3.0-only
 *
 * Adapted from luci-app-mosdns (GPL-3.0)
 * Original: https://github.com/sbwml/luci-app-mosdns
 * Copyright (C) sbwml <admin@cooluc.com>
 * Modified for rust-dns.
 *
 * The rpcd method luci.rust_dns print_log returns the tail (up to 256KB) of
 * the configured log file. Filtering is performed in the browser so the
 * backend does not need to parse log levels.
 */

const callPrintLog = rpc.declare({
	object: 'luci.rust_dns',
	method: 'print_log',
	expect: { '': {} }
});

const callCleanLog = rpc.declare({
	object: 'luci.rust_dns',
	method: 'clean_log',
	expect: { '': {} }
});

const LEVELS = [ 'all', 'error', 'warn', 'info', 'debug', 'trace' ];
const LEVEL_LABELS = {
	all: _('All levels'),
	error: _('Error'),
	warn: _('Warning'),
	info: _('Info'),
	debug: _('Debug'),
	trace: _('Trace')
};

let rawLog = '';
let lastError = null;
let logTextarea = null;
let logStatus = null;
let levelSelect = null;
let keywordInput = null;
let userScrolled = false;
let scrollPosition = 0;
let pollRegistered = false;
let visibilityRegistered = false;

function safeCall(fn, fallback) {
	try {
		return L.resolveDefault(fn(), fallback);
	}
	catch (e) {
		return Promise.resolve(fallback);
	}
}

function lineLevel(line) {
	const m = /\b(error|warn|warning|info|debug|trace)\b/i.exec(line || '');
	if (!m)
		return null;

	const lvl = m[1].toLowerCase();
	return lvl === 'warning' ? 'warn' : lvl;
}

function matchesFilters(line) {
	const level = levelSelect ? levelSelect.value : 'all';
	const keyword = keywordInput ? keywordInput.value.trim().toLowerCase() : '';

	if (level !== 'all' && lineLevel(line) !== level)
		return false;

	if (keyword && line.toLowerCase().indexOf(keyword) === -1)
		return false;

	return true;
}

function updateStatus(filteredCount, totalCount) {
	if (!logStatus)
		return;

	if (lastError) {
		logStatus.textContent = lastError;
		return;
	}

	if (totalCount === 0) {
		logStatus.textContent = _('No log data.');
		return;
	}

	logStatus.textContent = _('Showing %d of %d lines. Refresh every 5 seconds.')
		.format(filteredCount, totalCount);
}

function renderLog() {
	if (!logTextarea)
		return;

	const lines = rawLog.split(/\r?\n/);
	const filtered = lines.filter(matchesFilters);

	logTextarea.value = filtered.join('\n');
	updateStatus(filtered.length, lines.length);

	if (!userScrolled)
		logTextarea.scrollTop = logTextarea.scrollHeight;
	else
		logTextarea.scrollTop = scrollPosition;
}

function pollLog() {
	/* Skip RPC and DOM work while the tab is in the background. */
	if (document.visibilityState === 'hidden')
		return Promise.resolve();

	return safeCall(callPrintLog, { error: _('Log API is unavailable.') }).then(res => {
		res = res || {};

		if (res.log !== undefined && res.log !== null) {
			rawLog = String(res.log);
			lastError = null;
		}
		else {
			lastError = res.error || _('No log data.');
		}

		renderLog();
	});
}

function cleanLogs() {
	return safeCall(callCleanLog, { error: _('Log API is unavailable.') }).then(res => {
		res = res || {};

		if (res.success) {
			rawLog = '';
			lastError = null;
			renderLog();
			ui.addNotification(null, E('p', _('Logs cleared.')), 'info');
		}
		else {
			ui.addNotification(null,
				E('p', _('Failed to clean logs.') + (res.error ? ': ' + res.error : '')),
				'error');
		}
	}).catch(e => {
		ui.addNotification(null, E('p', _('Failed to clean logs: %s').format(e.message)), 'error');
	});
}

return view.extend({
	handleCleanLogs() {
		ui.showModal(_('Clear logs'), [
			E('p', {}, _('Are you sure you want to clear the log file? This cannot be undone.')),
			E('div', { 'class': 'right' }, [
				E('button', {
					'class': 'btn cbi-button cbi-button-neutral',
					'click': ui.hideModal
				}, _('Cancel')),
				' ',
				E('button', {
					'class': 'btn cbi-button cbi-button-remove',
					'click': () => {
						ui.hideModal();
						return cleanLogs();
					}
				}, _('Clear'))
			])
		]);
	},

	render() {
		rawLog = '';
		lastError = null;
		userScrolled = false;
		scrollPosition = 0;

		logTextarea = E('textarea', {
			'class': 'cbi-input-textarea',
			'wrap': 'off',
			'readonly': 'readonly',
			'style': 'width: 100%; box-sizing: border-box; height: 60vh; min-height: 280px; ' +
				'overflow-y: scroll; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; ' +
				'font-size: 12px;'
		});

		logTextarea.addEventListener('scroll', () => {
			const atBottom = logTextarea.scrollTop + logTextarea.clientHeight >= logTextarea.scrollHeight - 8;
			userScrolled = !atBottom;
			scrollPosition = logTextarea.scrollTop;
		});

		levelSelect = E('select', { 'class': 'cbi-input-select' },
			LEVELS.map(level => E('option', { 'value': level }, LEVEL_LABELS[level])));

		levelSelect.addEventListener('change', () => {
			userScrolled = false;
			renderLog();
		});

		keywordInput = E('input', {
			'type': 'text',
			'class': 'cbi-input-text',
			'placeholder': _('Filter by keyword...'),
			'style': 'min-width: 180px;'
		});

		keywordInput.addEventListener('input', () => {
			userScrolled = false;
			renderLog();
		});

		const clearButton = E('button', {
			'class': 'btn cbi-button cbi-button-action',
			'click': ev => this.handleCleanLogs(ev)
		}, _('Clear logs'));

		const followButton = E('button', {
			'class': 'btn cbi-button cbi-button-neutral',
			'click': () => {
				userScrolled = false;
				if (logTextarea)
					logTextarea.scrollTop = logTextarea.scrollHeight;
			}
		}, _('Follow tail'));

		logStatus = E('small', { 'style': 'display: block; margin-top: 0.5rem; opacity: 0.7;' }, '');

		/* poll timers survive view re-entry; register the reader only once. */
		if (!pollRegistered) {
			pollRegistered = true;
			poll.add(pollLog, 5);
		}

		if (!visibilityRegistered) {
			visibilityRegistered = true;
			document.addEventListener('visibilitychange', () => {
				if (document.visibilityState === 'visible')
					pollLog();
			});
		}
		pollLog();

		return E('div', { 'class': 'cbi-map' }, [
			E('h2', { 'name': 'content' }, '%s - %s'.format(_('rust-dns'), _('Log Data'))),
			E('div', { 'class': 'cbi-section' }, [
				E('div', {
					'style': 'display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; margin-bottom: 0.5rem;'
				}, [
					levelSelect,
					keywordInput,
					followButton,
					clearButton
				]),
				logTextarea,
				logStatus
			])
		]);
	},

	handleSave: null,
	handleSaveApply: null,
	handleReset: null
});
