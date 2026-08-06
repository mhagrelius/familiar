// The webcam's microphone processing, in Quick Settings.
//
// An AI webcam removes what it decides is noise, and what it decides is noise
// includes an assistant talking out of your speakers — and, it turns out, a
// good deal of anybody talking over it. That is the right behaviour in a
// meeting and the wrong behaviour when you are trying to interrupt something.
// Which means it is a thing you switch, which means it belongs beside Night
// Light rather than in an application's preferences.
//
// The switch runs `link-ctl`, which speaks to the camera over USB. There is no
// Linux version of Insta360's own Link Controller, and ALSA exposes nothing
// but capture volume for this device, so this is the only way to reach it.

import GObject from 'gi://GObject';
import Gio from 'gi://Gio';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';
import { QuickMenuToggle, SystemIndicator } from 'resource:///org/gnome/shell/ui/quickSettings.js';

import * as camera from './camera.js';

const ICON = 'audio-input-microphone-symbolic';

const WebcamMicToggle = GObject.registerClass(
class WebcamMicToggle extends QuickMenuToggle {
    _init() {
        super._init({
            title: 'Webcam Mic',
            iconName: ICON,
            toggleMode: true,
        });

        // Set while the switch is being moved to match the camera rather than
        // by somebody pressing it. Without it, reading the state back would
        // look like a press and write it straight out again.
        this._applying = false;

        // Cancels whatever link-ctl call is in flight when the panel goes away.
        // Without it the reply lands on a destroyed widget.
        this._cancellable = new Gio.Cancellable();

        this.menu.setHeader(ICON, 'Webcam Mic', 'Noise cancelling');

        const sound = new PopupMenu.PopupMenuItem('Sound Settings');
        sound.connect('activate', () => {
            Main.overview.hide();
            // Where the input device is chosen. Switching to a plain
            // microphone is the other half of this problem and GNOME already
            // does it well, so this points at it rather than repeating it.
            try {
                Gio.Subprocess.new(
                    ['gnome-control-center', 'sound'], Gio.SubprocessFlags.NONE);
            } catch (error) {
                Main.notifyError('Webcam Mic', error.message);
            }
        });
        this.menu.addMenuItem(sound);

        const refresh = new PopupMenu.PopupMenuItem('Re-read the Camera');
        refresh.connect('activate', () => this.refresh());
        this.menu.addMenuItem(refresh);

        this.connect('notify::checked', () => {
            if (this._applying)
                return;
            this._write(this.checked);
        });

        this.refresh();
    }

    // What the camera says, which is the only thing worth showing. A switch
    // that shows what was last asked for rather than what is true is a switch
    // that lies after the camera has been unplugged.
    refresh() {
        if (!camera.tool()) {
            this._unavailable('link-ctl is not installed');
            return;
        }
        camera.readNoiseCancelling(this._cancellable, result => {
            if (!result.ok) {
                this._unavailable(result.error);
                return;
            }
            this.reactive = true;
            this._show(result.value);
        });
    }

    _write(on) {
        camera.setNoiseCancelling(on, this._cancellable, result => {
            if (!result.ok) {
                this._unavailable(result.error);
                return;
            }
            // Read it back rather than trust the write: this is a camera on
            // the end of a USB cable, not a variable.
            this.refresh();
        });
    }

    _show(on) {
        this._applying = true;
        this.checked = on;
        this._applying = false;
        this.subtitle = on ? 'Noise cancelling on' : 'Noise cancelling off';
        this.menu.setHeader(
            ICON, 'Webcam Mic',
            on ? 'Removing background noise' : 'Passing everything through');
    }

    // Insensitive and explained, never inert. A toggle that latches while
    // nothing happens reads as a bug in the shell.
    _unavailable(why) {
        this._applying = true;
        this.checked = false;
        this._applying = false;
        this.reactive = false;
        this.subtitle = why;
        this.menu.setHeader(ICON, 'Webcam Mic', why);
    }

    destroy() {
        this._cancellable.cancel();
        this._cancellable = null;
        super.destroy();
    }
});

const Indicator = GObject.registerClass(
class Indicator extends SystemIndicator {
    _init() {
        super._init();
        this._toggle = new WebcamMicToggle();
        this.quickSettingsItems.push(this._toggle);
    }
});

export default class WebcamMicExtension extends Extension {
    enable() {
        this._indicator = new Indicator();
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
    }

    disable() {
        this._indicator?.quickSettingsItems.forEach(item => item.destroy());
        this._indicator?.destroy();
        this._indicator = null;
    }
}
