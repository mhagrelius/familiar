// The Quick Settings entry itself.
//
// The toggle's subtitle is the whole point of glancing at it: what runs next,
// or how many runs are owed. Expanding gives a row per job with a switch, so
// pausing a briefing before a week away is two clicks and does not mean finding
// the chat it lives in.

import GObject from 'gi://GObject';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import { QuickMenuToggle, SystemIndicator } from 'resource:///org/gnome/shell/ui/quickSettings.js';

const ICON = 'alarm-symbolic';

/// "in 4 hours", "tomorrow at 07:00" — coarse on purpose, the way a status line
/// wants it.
function when(iso) {
    if (!iso)
        return 'not scheduled';
    const at = GLib.DateTime.new_from_iso8601(iso, null);
    if (!at)
        return 'not scheduled';
    const now = GLib.DateTime.new_now_local();
    const minutes = Math.round((at.to_unix() - now.to_unix()) / 60);
    if (minutes <= 0)
        return 'due now';
    if (minutes < 60)
        return `in ${minutes} min`;
    if (minutes < 60 * 24)
        return `at ${at.format('%H:%M')}`;
    return at.format('%a at %H:%M');
}

const FamiliarToggle = GObject.registerClass(
class FamiliarToggle extends QuickMenuToggle {
    _init(jobs) {
        super._init({
            title: 'Familiar',
            iconName: ICON,
            toggleMode: false,
        });
        this._jobs = jobs;

        this.menu.setHeader(ICON, 'Familiar', 'Scheduled chats');
        this._section = new PopupMenu.PopupMenuSection();
        this.menu.addMenuItem(this._section);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const open = new PopupMenu.PopupMenuItem('Open Familiar');
        open.connect('activate', () => {
            Main.overview.hide();
            // By desktop file rather than by spawning a binary: the app may be
            // a flatpak, and only the launcher knows how to start it.
            const app = Gio.DesktopAppInfo.new('us.hagreli.Familiar.desktop');
            if (!app)
                return;
            try {
                app.launch([], null);
            } catch (error) {
                Main.notifyError('Familiar', error.message);
            }
        });
        this.menu.addMenuItem(open);

        this._changedId = this._jobs.connect('changed', () => this._redraw());
        this._runningId = this._jobs.connect('notify::running', () => this._redraw());
        this._redraw();
    }

    _redraw() {
        this._section.removeAll();

        // Not running is an ordinary state for a desktop app, and saying so is
        // better than an empty list — which reads as "you have no schedules".
        if (!this._jobs.running) {
            this.subtitle = 'Not running';
            this.checked = false;
            this._note('Familiar is not running');
            return;
        }

        const jobs = this._jobs.jobs;
        if (jobs.length === 0) {
            this.subtitle = 'Nothing scheduled';
            this.checked = false;
            this._note('No scheduled chats');
            return;
        }

        const live = jobs.filter(job => job.enabled);
        const overdue = this._jobs.overdue;
        // Filled while something is actually scheduled to run, hollow when
        // everything is paused — the state reads without opening anything,
        // which is llama-tray's rule and the right one.
        this.checked = live.length > 0;
        if (overdue > 0) {
            this.subtitle = overdue === 1 ? '1 run due' : `${overdue} runs due`;
        } else if (live.length === 0) {
            this.subtitle = 'All paused';
        } else {
            const next = live
                .filter(job => job.nextRun)
                .sort((a, b) => a.nextRun.localeCompare(b.nextRun))[0];
            this.subtitle = next ? `Next ${when(next.nextRun)}` : `${live.length} scheduled`;
        }

        for (const job of jobs)
            this._addJob(job);
    }

    _addJob(job) {
        const row = new PopupMenu.PopupSwitchMenuItem(job.title, job.enabled);
        row.label.add_style_class_name('familiar-job-title');
        row.connect('toggled', (_item, state) => {
            this._jobs.setEnabled(job.id, state).then(outcome => {
                if (!outcome.ok)
                    Main.notifyError('Familiar', outcome.message);
            });
        });
        this._section.addMenuItem(row);

        // What it does and when, under the name — a title alone does not tell
        // you whether this is the one you meant to pause.
        const detail = job.enabled
            ? `${job.schedule} · next ${when(job.nextRun)}`
            : `${job.schedule} · paused`;
        const under = new PopupMenu.PopupMenuItem(detail);
        under.setSensitive(false);
        under.label.add_style_class_name('familiar-job-detail');
        this._section.addMenuItem(under);
    }

    _note(text) {
        const note = new PopupMenu.PopupMenuItem(text);
        note.setSensitive(false);
        this._section.addMenuItem(note);
    }

    destroy() {
        this._jobs.disconnect(this._changedId);
        this._changedId = 0;
        this._jobs.disconnect(this._runningId);
        this._runningId = 0;
        this._jobs = null;
        super.destroy();
    }
});

export const FamiliarIndicator = GObject.registerClass(
class FamiliarIndicator extends SystemIndicator {
    _init(jobs) {
        super._init();
        this._toggle = new FamiliarToggle(jobs);
        this.quickSettingsItems.push(this._toggle);
    }
});
