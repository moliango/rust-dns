'use strict';
'require form';
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
 * The update tool itself is provided by the rust-dns service package
 * (/usr/share/rust-dns/rust-dns.uc). This page only writes the UCI options
 * and drives start_update / get_update_log exposed by luci.rust_dns.
 */

const callStartUpdate = rpc.declare({
	object: 'luci.rust_dns',
	method: 'start_update',
	expect: { '': {} }
});

const callGetUpdateLog = rpc.declare({
	object: 'luci.rust_dns',
	method: 'get_update_log',
	expect: { '': {} }
});

const UPDATE_EXIT_RE = /^UPDATE_EXITED\s+(\d+)\s*$/m;
const UPDATE_FINISHED_RE = /UPDATE_(FINISHED|SUCCESS)/;
const UPDATE_FAILED_RE = /UPDATE_(EXITED|FAILED)/;
const UPDATE_BUSY_RE = /Another update is already in progress/i;
const UPDATE_TIMEOUT_MS = 30 * 60 * 1000;

return view.extend({
	handleUpdate() {
		const statusMsg = E('p', { 'class': 'spinning' }, _('Please wait, this may take a few moments...'));

		const logTextarea = E('textarea', {
			'class': 'cbi-input-textarea',
			'readonly': 'readonly',
			'style': 'width: 100%; box-sizing: border-box; height: 300px; ' +
				'font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace; ' +
				'font-size: 12px; margin-top: 10px;',
			'placeholder': _('Starting update...')
		});

		let interval = null;
		const startedAt = Date.now();

		const stopPolling = () => {
			if (interval !== null) {
				window.clearInterval(interval);
				interval = null;
			}
		};

		const done = (message, color, bold) => {
			stopPolling();
			statusMsg.textContent = message;
			statusMsg.classList.remove('spinning');
			statusMsg.style.color = color;
			statusMsg.style.fontWeight = bold ? 'bold' : 'normal';
			closeButton.style.display = 'inline';
		};

		const closeButton = E('button', {
			'class': 'btn',
			'style': 'display: none;',
			'click': () => {
				stopPolling();
				ui.hideModal();
			}
		}, _('Close'));

		ui.showModal(_('Updating database...'), [
			statusMsg,
			logTextarea,
			E('div', { 'class': 'right' }, [ closeButton ])
		]);

		const applyLog = res => {
			if (res && res.log) {
				logTextarea.value = res.log;
				logTextarea.scrollTop = logTextarea.scrollHeight;
				return true;
			}
			return false;
		};

		const pollLog = () => {
			return L.resolveDefault(callGetUpdateLog(), { error: _('Update log API is unavailable.') }).then(res => {
				if (res && res.error && !res.log) {
					done(_('Failed to read update log: %s').format(res.error), '#ed4014', true);
					return false;
				}

				const hasLog = applyLog(res);

				if (Date.now() - startedAt > UPDATE_TIMEOUT_MS) {
					done(_('Update is still running. Check the log for progress.'), '#ff9900', true);
					return false;
				}

				if (!hasLog)
					return true;

				const exitMatch = res.log.match(UPDATE_EXIT_RE);
				if (exitMatch) {
					const code = parseInt(exitMatch[1], 10);
					if (code === 0)
						done(_('Update success'), '#19be6b', true);
					else
						done(_('Update failed'), '#ed4014', true);
					return false;
				}

				if (UPDATE_FINISHED_RE.test(res.log)) {
					done(_('Update success'), '#19be6b', true);
					return false;
				}

				if (UPDATE_FAILED_RE.test(res.log)) {
					done(_('Update failed'), '#ed4014', true);
					return false;
				}

				if (UPDATE_BUSY_RE.test(res.log)) {
					done(_('Another update is already in progress.'), '#ff9900', true);
					return false;
				}

				return true;
			}).catch(e => {
				done(_('Failed to read update log: %s').format(e.message), '#ed4014', true);
				return false;
			});
		};

		return L.resolveDefault(callStartUpdate(), { error: _('Update API is unavailable.') }).then(res => {
			res = res || {};

			if (res.success) {
				interval = window.setInterval(() => {
					pollLog().then(continuePolling => {
						if (!continuePolling)
							stopPolling();
					});
				}, 1000);
			}
			else if (res.error && UPDATE_BUSY_RE.test(res.error)) {
				statusMsg.textContent = _('Another update is already in progress.');
				statusMsg.classList.remove('spinning');
				statusMsg.style.color = '#ff9900';
				closeButton.style.display = 'inline';
				L.resolveDefault(callGetUpdateLog(), {}).then(applyLog).catch(() => {});
			}
			else {
				statusMsg.textContent = res.error || _('Failed to start update.');
				statusMsg.classList.remove('spinning');
				statusMsg.style.color = '#ed4014';
				closeButton.style.display = 'inline';
			}
		}).catch(e => {
			statusMsg.textContent = _('Update failed: %s').format(e.message);
			statusMsg.classList.remove('spinning');
			statusMsg.style.color = '#ed4014';
			closeButton.style.display = 'inline';
		});
	},

	render() {
		let m, s, o;

		m = new form.Map('rust-dns', _('Update GeoIP & GeoSite databases'),
			_('Automatically update GeoIP and GeoSite databases as well as ad filtering rules through scheduled tasks.'));

		s = m.section(form.NamedSection, 'config', 'rust-dns');

		o = s.option(form.Flag, 'geo_auto_update', _('Enable Auto Database Update'));
		o.rmempty = false;

		o = s.option(form.ListValue, 'geo_update_week_time', _('Update Cycle'));
		o.value('*', _('Every Day'));
		o.value('1', _('Every Monday'));
		o.value('2', _('Every Tuesday'));
		o.value('3', _('Every Wednesday'));
		o.value('4', _('Every Thursday'));
		o.value('5', _('Every Friday'));
		o.value('6', _('Every Saturday'));
		o.value('0', _('Every Sunday'));
		o.default = '*';

		o = s.option(form.ListValue, 'geo_update_day_time', _('Update Time'));
		for (let t = 0; t < 24; t++)
			o.value(t, t + ':00');
		o.default = '2';

		o = s.option(form.ListValue, 'geoip_type', _('GeoIP Type'),
			_('Little: only include Mainland China and Private IP addresses.') +
			'<br>' +
			_('Full: includes all Countries and Private IP addresses.'));
		o.value('geoip', _('Full'));
		o.value('geoip-only-cn-private', _('Little'));
		o.rmempty = false;
		o.default = 'geoip-only-cn-private';

		o = s.option(form.Value, 'github_proxy', _('GitHub Proxy'),
			_('Update data files with GitHub Proxy, leave blank to disable proxy downloads.'));
		o.value('https://gh-proxy.com', _('https://gh-proxy.com'));
		o.rmempty = true;
		o.default = '';

		o = s.option(form.Button, '_update', null, _('Check and update GeoData.'));
		o.title = _('Database Update');
		o.inputtitle = _('Check And Update');
		o.inputstyle = 'apply';
		o.onclick = () => this.handleUpdate();

		return m.render();
	}
});
