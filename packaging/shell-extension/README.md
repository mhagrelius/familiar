# Familiar for GNOME Shell

Familiar's schedules in the Quick Settings panel: what runs next, what happened
last time, and a switch to pause anything.

```
▾ Familiar · Next at 07:00
┌──────────────────────────────┐
│ Morning Briefing        [on] │
│   Daily at 07:00 · next 07:00│
│ Weekly PR Review        [on] │
│   Mondays at 09:00 · next Mon│
├──────────────────────────────┤
│ Open Familiar                │
└──────────────────────────────┘
```

The toggle is filled while something is actually scheduled and hollow when
everything is paused, so the state reads without opening anything.

## Why an extension rather than a tray icon

A `StatusNotifierItem` tray needs `ubuntu-appindicators` or an equivalent to
have anywhere to appear. Quick Settings is where GNOME itself puts things that
are *on* rather than things you *open*, which is what a scheduled assistant is.

**Familiar is complete without this.** It runs its schedules, raises
notifications and manages them in its own window whether or not the extension is
installed. This is a surface over the `us.hagreli.Familiar.Jobs` D-Bus
interface, and the interface is the thing that matters — a tray, a panel applet
or a shell script would read exactly the same one.

## Install

```sh
make install
gnome-extensions enable familiar@hagreli.us
```

On Wayland the shell only discovers a newly installed extension at startup, so
log out and back in the first time.

## How it talks to Familiar

`Gio.DBusProxy` on the session bus, `DO_NOT_AUTO_START`, and
`Gio.bus_watch_name` rather than a single check at startup — so the panel follows
Familiar being started and quit without anybody reloading the shell.

**No timer.** `g-properties-changed` is what says a schedule changed; Familiar
emits `PropertiesChanged` whenever a job is added, edited, paused, deleted or
finishes a run. Polling a local model app every couple of seconds to ask
"anything scheduled?" would cost more than the panel shows.

Familiar not running is an ordinary state for a desktop app, and the menu says
so rather than showing an empty list — which would read as "you have no
schedules".

Failures cross the D-Bus boundary as values: `daemon/result.js` defines
`ok`/`err`, and everything above `daemon/` branches on `ok`. An extension that
throws out of an async handler takes a notification with it and tells the user
nothing.

## Layout

```
daemon/     the one external seam
  result.js   typed success/failure
  jobs.js     the D-Bus proxy, as a GObject the UI observes
extension.js  the Quick Settings toggle and its menu
```

## Development

```sh
make lint      # every file parses; a syntax error here takes the panel down
make logs      # follow this extension's shell output
make pack      # zip for extensions.gnome.org
```

To try changes without touching your session, run a headless shell against an
isolated dconf profile:

```sh
gnome-shell --headless --virtual-monitor 1400x900 --wayland-display=wayland-fam
```

`gnome-shell --nested` was removed in GNOME 50; nesting is the default and
`--headless` is what works without taking over the seat.

## Checking the interface by hand

The extension is a view. Everything it does can be done with `gdbus`, which is
the quickest way to tell an extension bug from an app bug:

```sh
gdbus introspect --session --dest us.hagreli.Familiar \
  --object-path /us/hagreli/Familiar/Jobs

gdbus call --session --dest us.hagreli.Familiar \
  --object-path /us/hagreli/Familiar/Jobs \
  --method us.hagreli.Familiar.Jobs.List
```
