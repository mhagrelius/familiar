// Success and failure as values.
//
// Everything above `daemon/` branches on `ok` rather than catching. The seam is
// the D-Bus call, and a shell extension that throws out of an async handler
// takes a notification with it and tells the user nothing — so failure crosses
// the boundary as data and the menu decides what to say about it.

export function ok(value) {
    return { ok: true, value };
}

export function err(message) {
    return { ok: false, message: String(message) };
}

/// Run something that may throw and get a result back instead.
export async function attempt(work) {
    try {
        return ok(await work());
    } catch (error) {
        return err(error?.message ?? error);
    }
}
