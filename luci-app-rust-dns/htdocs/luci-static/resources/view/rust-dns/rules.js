'use strict';
'require form';
'require fs';
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
 * Rule files are fixed to /etc/rust-dns/rule/; the UI never accepts an
 * arbitrary path. Saving writes through fs.write() and then asks the fixed
 * init script to reload the service.
 */

const RULE_DIR = '/etc/rust-dns/rule/';

const RULES = [
	{
		name: 'whitelist',
		title: _('White Lists'),
		file: RULE_DIR + 'whitelist.txt',
		desc: _('Added domain names always permit resolution using \'Local DNS\' with the highest priority (one domain per line, supports domain matching rules).')
	},
	{
		name: 'blocklist',
		title: _('Block Lists'),
		file: RULE_DIR + 'blocklist.txt',
		desc: _('Added domain names will block DNS resolution (one domain per line, supports domain matching rules).')
	},
	{
		name: 'greylist',
		title: _('Grey Lists'),
		file: RULE_DIR + 'greylist.txt',
		desc: _('Added domain names will always use \'Remote DNS\' for resolution (one domain per line, supports domain matching rules).')
	},
	{
		name: 'ddnslist',
		title: _('DDNS Lists'),
		file: RULE_DIR + 'ddnslist.txt',
		desc: _('Added domain names will always use \'Local DNS\' for resolution with a forced TTL of 5 seconds and will not be cached (one domain per line, supports domain matching rules).')
	},
	{
		name: 'hosts',
		title: _('Hosts'),
		file: RULE_DIR + 'hosts.txt',
		desc: _('Custom hosts rewrite, for example: baidu.com 10.0.0.1 (one rule per line, supports domain matching rules).')
	},
	{
		name: 'redirect',
		title: _('Redirect'),
		file: RULE_DIR + 'redirect.txt',
		desc: _('Redirect requests for one domain to another, for example: baidu.com qq.com (one rule per line).')
	},
	{
		name: 'local-ptr',
		title: _('Block PTR'),
		file: RULE_DIR + 'local-ptr.txt',
		desc: _('Added domain names will block PTR requests (one domain per line, supports domain matching rules).')
	},
	{
		name: 'streaming',
		title: _('Streaming Media'),
		file: RULE_DIR + 'streaming.txt',
		desc: _('When \'Custom Stream Media DNS\' is enabled, added domains always use the streaming media DNS server for resolution (one domain per line, supports domain matching rules).')
	}
];

/* Reload through the fixed init script. Missing ACL / mock environment must
 * not break the page, so failures resolve to null instead of rejecting. */
function reloadService() {
	try {
		return L.resolveDefault(fs.exec('/etc/init.d/rust-dns', [ 'reload' ]), null).catch(() => null);
	}
	catch (e) {
		return Promise.resolve(null);
	}
}

function notifyReload(res) {
	if (res && res.code === 0)
		ui.addNotification(null, E('p', _('Rules saved. The rust-dns service was asked to reload.')), 'info');
	else
		ui.addNotification(null, E('p', _('Rules saved, but the service could not be reloaded. Please restart rust-dns manually.')), 'warning');
}

/* Validate a single manually entered rule line. Newlines and control
 * characters would let one entry inject additional lines, so they are
 * rejected; runs of spaces are collapsed but a single field separator is
 * still allowed for hosts/redirect rules. */
function normalizeEntry(raw) {
	const value = String(raw == null ? '' : raw);

	if (/[\r\n\t\u0000-\u001f\u007f\u2028\u2029]/.test(value))
		return { error: _('Entry contains invalid characters. Newlines, tabs and control characters are not allowed.') };

	const trimmed = value.trim();

	if (trimmed === '')
		return { error: _('Entry cannot be empty.') };

	if (trimmed.length > 1024)
		return { error: _('Entry is too long.') };

	return { value: trimmed.replace(/ {2,}/g, ' ') };
}

function readEntryLines(file) {
	return fs.read(file).catch(() => '').then(content => {
		return String(content || '').replace(/\r\n/g, '\n').split('\n');
	});
}

function appendEntry(file, entry) {
	return readEntryLines(file).then(lines => {
		const needle = entry.toLowerCase();

		for (const line of lines) {
			if (line.trim().toLowerCase() === needle)
				return { exists: true };
		}

		let content = lines.join('\n');

		if (content !== '' && !content.endsWith('\n'))
			content += '\n';

		return fs.write(file, content + entry + '\n').then(() => ({ exists: false }));
	});
}

function deleteEntry(file, entry) {
	return readEntryLines(file).then(lines => {
		const needle = entry.toLowerCase();
		const kept = [];
		let removed = 0;

		for (const line of lines) {
			const trimmed = line.trim();

			if (trimmed !== '' && trimmed.toLowerCase() === needle) {
				removed++;
				continue;
			}

			kept.push(line);
		}

		if (removed === 0)
			return { found: false };

		let content = kept.join('\n').replace(/\n+$/, '');
		content = (content === '') ? '' : content + '\n';

		return fs.write(file, content).then(() => ({ found: true }));
	});
}

function showEntryModal(rule, mode) {
	const isAdd = (mode === 'add');
	const input = E('input', {
		'class': 'cbi-input-text',
		'type': 'text',
		'style': 'width: 100%; box-sizing: border-box;',
		'placeholder': _('Enter a single entry')
	});
	const errorNode = E('p', { style: 'color: #c00; min-height: 1.2em; margin-bottom: 0;' }, '');
	const submit = E('button', {
		'class': 'btn cbi-button ' + (isAdd ? 'cbi-button-apply' : 'cbi-button-remove')
	}, isAdd ? _('Add entry') : _('Remove entry'));

	const run = () => {
		const normalized = normalizeEntry(input.value);

		if (normalized.error) {
			errorNode.textContent = normalized.error;
			return;
		}

		submit.disabled = true;

		const operation = isAdd
			? appendEntry(rule.file, normalized.value)
			: deleteEntry(rule.file, normalized.value);

		operation.then(result => {
			if (isAdd && result.exists) {
				errorNode.textContent = _('Entry is already present in %s.').format(rule.file);
				submit.disabled = false;
				return;
			}

			if (!isAdd && !result.found) {
				errorNode.textContent = _('Entry was not found in %s.').format(rule.file);
				submit.disabled = false;
				return;
			}

			ui.hideModal();

			return reloadService().then(res => {
				if (res && res.code === 0) {
					ui.addNotification(null, E('p',
						(isAdd ? _('%s added to %s.') : _('%s removed from %s.'))
							.format(normalized.value, rule.file)), 'info');
				} else {
					ui.addNotification(null, E('p',
						(isAdd
							? _('%s added to %s, but the service could not be reloaded. Please restart rust-dns manually.')
							: _('%s removed from %s, but the service could not be reloaded. Please restart rust-dns manually.'))
							.format(normalized.value, rule.file)), 'warning');
				}

				window.setTimeout(() => window.location.reload(), 800);
			});
		}).catch(err => {
			errorNode.textContent = _('Unable to update %s: %s').format(rule.file, err.message);
			submit.disabled = false;
		});
	};

	submit.addEventListener('click', run);
	input.addEventListener('keydown', ev => {
		if (ev.key === 'Enter') {
			ev.preventDefault();
			run();
		}
	});

	ui.showModal(isAdd ? _('Add rule entry') : _('Remove rule entry'), [
		E('p', {}, _('File: %s').format(rule.file)),
		E('p', {}, _('Enter a single entry. Newlines, tabs and control characters are not allowed.')),
		input,
		errorNode,
		E('div', { class: 'right' }, [
			E('button', { 'class': 'btn', 'click': ui.hideModal }, _('Cancel')),
			' ',
			submit
		])
	]);

	input.focus();
}

return view.extend({
	render() {
		const m = new form.Map('rust-dns', _('Rule Settings'),
			_('The rule files below are used by the generated configuration. One entry per line.') + ' ' +
			_('Saving with "Save & Apply" writes every edited file and reloads the rust-dns service.'));

		const s = m.section(form.NamedSection, 'config', 'rust-dns');

		const handleSaveError = e => {
			ui.addNotification(null, E('p', _('Unable to save contents: %s').format(e.message)), 'error');
		};

		RULES.forEach(rule => {
			s.tab(rule.name, rule.title);

			const o = s.taboption(rule.name, form.TextValue, '_' + rule.name, null, rule.desc);
			o.rows = 25;
			o.monospace = true;
			o.cfgvalue = () => fs.trimmed(rule.file).catch(() => '');
			o.write = function(section_id, formvalue) {
				return this.cfgvalue(section_id).then(value => {
					if (value === formvalue)
						return;

					const content = (formvalue && formvalue.trim())
						? formvalue.trim().replace(/\r\n/g, '\n') + '\n'
						: '';

					return fs.write(rule.file, content).catch(handleSaveError);
				});
			};

			const addBtn = s.taboption(rule.name, form.Button, '_add_' + rule.name);
			addBtn.inputtitle = _('Add entry');
			addBtn.inputstyle = 'apply';
			addBtn.onclick = () => showEntryModal(rule, 'add');

			const removeBtn = s.taboption(rule.name, form.Button, '_remove_' + rule.name);
			removeBtn.inputtitle = _('Remove entry');
			removeBtn.inputstyle = 'remove';
			removeBtn.onclick = () => showEntryModal(rule, 'remove');

			const clearBtn = s.taboption(rule.name, form.Button, '_clear_' + rule.name);
			clearBtn.inputtitle = _('Clear this file');
			clearBtn.inputstyle = 'remove';
			clearBtn.onclick = () => {
				ui.showModal(_('Clear rule file'), [
					E('p', {}, _('Are you sure you want to clear %s? This cannot be undone.').format(rule.file)),
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
								return fs.write(rule.file, '').then(() => {
									return reloadService();
								}).then(res => {
									if (res && res.code === 0)
										ui.addNotification(null, E('p', _('%s cleared, reloading the page.').format(rule.file)), 'info');
									else
										ui.addNotification(null,
											E('p', _('%s cleared, but the service could not be reloaded. Please restart rust-dns manually.').format(rule.file)),
											'warning');
									window.setTimeout(() => window.location.reload(), 800);
								}).catch(handleSaveError);
							}
						}, _('Clear'))
					])
				]);
			};
		});

		return m.render();
	},

	handleSaveApply(ev) {
		return this.handleSave(ev).then(() => {
			return reloadService().then(notifyReload);
		}).catch(e => {
			ui.addNotification(null, E('p', _('Failed to save rules: %s').format(e.message)), 'error');
		});
	},

	handleReset: null
});
