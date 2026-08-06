// Familiar's schedules, in Quick Settings.
//
// Beside Night Light and the VPN, because that is where the system puts things
// that are *on* rather than things you *open* — which is what a scheduled
// assistant is.

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

import { Jobs } from './daemon/jobs.js';
import { FamiliarIndicator } from './ui/toggle.js';

export default class FamiliarExtension extends Extension {
    enable() {
        this._jobs = new Jobs();
        this._indicator = new FamiliarIndicator(this._jobs);
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
    }

    disable() {
        this._indicator.quickSettingsItems.forEach(item => item.destroy());
        this._indicator.destroy();
        this._indicator = null;

        this._jobs.destroy();
        this._jobs = null;
    }
}
