// The one external seam: `link-ctl`, which talks to the webcam over USB.
//
// Everything above this file deals in `{ok: true, value}` / `{ok: false,
// error}` and never in exit codes or stderr. An extension that throws out of
// an async handler takes a notification with it and tells the user nothing.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

// gnome-shell does not reliably inherit ~/.local/bin on its PATH — it is
// started by the session, not by a login shell — so the place `uv tool install`
// puts things is checked by hand before falling back to a search.
export function tool() {
    const local = GLib.build_filenamev([GLib.get_home_dir(), '.local', 'bin', 'link-ctl']);
    if (GLib.file_test(local, GLib.FileTest.IS_EXECUTABLE))
        return local;
    return GLib.find_program_in_path('link-ctl');
}

// Run one command and hand back what it said. Never throws: a camera that has
// been unplugged is an ordinary state, not an error to crash the panel with.
//
// The caller passes its own cancellable and `onDone` is not called once that is
// cancelled — the camera is on the end of a USB cable and a call can still be
// in flight when the panel goes away.
function run(argv, cancellable, onDone) {
    const program = tool();
    if (!program) {
        onDone({ok: false, error: 'link-ctl is not installed'});
        return;
    }
    let process;
    try {
        process = Gio.Subprocess.new(
            [program, ...argv],
            Gio.SubprocessFlags.STDOUT_PIPE | Gio.SubprocessFlags.STDERR_PIPE);
    } catch (error) {
        onDone({ok: false, error: error.message});
        return;
    }
    process.communicate_utf8_async(null, cancellable, (source, result) => {
        try {
            const [, stdout, stderr] = source.communicate_utf8_finish(result);
            if (!source.get_successful()) {
                onDone({ok: false, error: (stderr || '').trim() || 'the camera did not answer'});
                return;
            }
            onDone({ok: true, value: (stdout || '').trim()});
        } catch (error) {
            if (error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                return;
            onDone({ok: false, error: error.message});
        }
    });
}

/// Whether the camera's AI noise cancelling is on.
export function readNoiseCancelling(cancellable, onDone) {
    run(['noise-cancel', 'status'], cancellable, result => {
        if (!result.ok) {
            onDone(result);
            return;
        }
        // `status` prints `on` or `off` and nothing else.
        onDone({ok: true, value: result.value.endsWith('on')});
    });
}

/// Turn it on or off.
export function setNoiseCancelling(on, cancellable, onDone) {
    run(['noise-cancel', on ? 'on' : 'off'], cancellable, onDone);
}
