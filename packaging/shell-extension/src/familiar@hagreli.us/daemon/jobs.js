// The one external seam: Familiar's Jobs interface over the session bus.
//
// **Push, never poll.** The proxy's `g-properties-changed` is what tells this
// extension a schedule changed, exactly as tailscale-gnome holds the IPN bus
// open rather than running a timer. A panel that woke every couple of seconds
// to ask a local model app "anything scheduled?" would cost more than it shows.
//
// Familiar may not be running, and usually is not — it is a desktop app, not a
// service. That is an ordinary state rather than an error: the proxy is created
// with DO_NOT_AUTO_START so watching costs nothing, and the menu says the app
// is not running rather than showing an empty list that reads as "no schedules".

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';

import { attempt, err, ok } from './result.js';

const NAME = 'us.hagreli.Familiar';
const PATH = '/us/hagreli/Familiar/Jobs';
const IFACE = 'us.hagreli.Familiar.Jobs';

const XML = `
<node>
  <interface name="${IFACE}">
    <method name="List">
      <arg type="aa{sv}" name="jobs" direction="out"/>
    </method>
    <method name="SetEnabled">
      <arg type="s" name="id" direction="in"/>
      <arg type="b" name="on" direction="in"/>
      <arg type="b" name="found" direction="out"/>
    </method>
    <method name="RunNow">
      <arg type="s" name="id" direction="in"/>
      <arg type="b" name="started" direction="out"/>
    </method>
    <property name="Jobs" type="aa{sv}" access="read"/>
    <property name="Overdue" type="u" access="read"/>
  </interface>
</node>`;

const Proxy = Gio.DBusProxy.makeProxyWrapper(XML);

/// One job, unpacked out of its `a{sv}` into something the menu can read.
///
/// Every field is defaulted. The dictionary is deliberately open so Familiar can
/// add one without breaking an extension that updates separately — which only
/// works if reading a missing key is not a crash.
function unpack(entry) {
    const at = (key, fallback) =>
        entry[key] ? entry[key].deepUnpack() ?? fallback : fallback;
    return {
        id: at('id', ''),
        title: at('title', 'Untitled job'),
        schedule: at('schedule', ''),
        prompt: at('prompt', ''),
        enabled: at('enabled', true),
        recovery: at('recovery', ''),
        nextRun: at('next_run', ''),
        lastRun: at('last_run', ''),
        lastOutcome: at('last_outcome', ''),
        project: at('project', ''),
        chat: at('chat', ''),
    };
}

export const Jobs = GObject.registerClass(
    {
        Signals: { 'changed': {} },
        Properties: {
            'running': GObject.ParamSpec.boolean(
                'running', '', '', GObject.ParamFlags.READABLE, false),
        },
    },
    class Jobs extends GObject.Object {
        _init() {
            super._init();
            this._proxy = null;
            this._jobs = [];
            this._running = false;

            // Watched rather than checked once, so the panel follows Familiar
            // being started and stopped without anybody reloading the shell —
            // the same reason llama-tray watches for StatusNotifierWatcher
            // instead of looking for it at startup.
            this._watch = Gio.bus_watch_name(
                Gio.BusType.SESSION,
                NAME,
                Gio.BusNameWatcherFlags.NONE,
                () => this._appeared(),
                () => this._vanished());
        }

        get running() {
            return this._running;
        }

        get jobs() {
            return this._jobs;
        }

        /// Jobs that are due right now, which is what the icon reflects.
        get overdue() {
            const now = GLib.DateTime.new_now_local().to_unix();
            return this._jobs.filter(job => {
                if (!job.enabled || !job.nextRun)
                    return false;
                const next = GLib.DateTime.new_from_iso8601(job.nextRun, null);
                return next && next.to_unix() <= now;
            }).length;
        }

        _appeared() {
            this._proxy = new Proxy(
                Gio.DBus.session, NAME, PATH,
                (proxy, error) => {
                    if (error) {
                        this._running = false;
                        this.notify('running');
                        this.emit('changed');
                        return;
                    }
                    this._running = true;
                    this.notify('running');
                    this.refresh();
                },
                null,
                Gio.DBusProxyFlags.DO_NOT_AUTO_START);

            // The push. Nothing here has a timer.
            this._changedId = this._proxy?.connect('g-properties-changed', () => {
                this._readProperty();
            });
        }

        _vanished() {
            if (this._proxy && this._changedId)
                this._proxy.disconnect(this._changedId);
            this._proxy = null;
            this._changedId = null;
            this._running = false;
            this._jobs = [];
            this.notify('running');
            this.emit('changed');
        }

        _readProperty() {
            const packed = this._proxy?.Jobs;
            if (!packed)
                return;
            this._jobs = packed.map(unpack);
            this.emit('changed');
        }

        /// Ask for the list outright, for the first fill.
        async refresh() {
            if (!this._proxy)
                return err('Familiar is not running');
            const outcome = await attempt(() =>
                new Promise((resolve, reject) => {
                    this._proxy.ListRemote((result, error) => {
                        if (error)
                            reject(error);
                        else
                            resolve(result[0]);
                    });
                }));
            if (!outcome.ok)
                return outcome;
            this._jobs = outcome.value.map(unpack);
            this.emit('changed');
            return ok(this._jobs);
        }

        async setEnabled(id, on) {
            if (!this._proxy)
                return err('Familiar is not running');
            return attempt(() =>
                new Promise((resolve, reject) => {
                    this._proxy.SetEnabledRemote(id, on, (result, error) => {
                        if (error)
                            reject(error);
                        else
                            resolve(result[0]);
                    });
                }));
        }

        async runNow(id) {
            if (!this._proxy)
                return err('Familiar is not running');
            return attempt(() =>
                new Promise((resolve, reject) => {
                    this._proxy.RunNowRemote(id, (result, error) => {
                        if (error)
                            reject(error);
                        else
                            resolve(result[0]);
                    });
                }));
        }

        destroy() {
            if (this._watch)
                Gio.bus_unwatch_name(this._watch);
            this._watch = null;
            this._vanished();
        }
    });
