# Webcam Mic

An Insta360 Link webcam's AI noise cancelling, as a switch in Quick Settings.

```
┌──────────────────────────────┐
│ 🎤 Webcam Mic           [on] │
│    Noise cancelling on       │
└──────────────────────────────┘
```

## Why this exists

An AI webcam removes what it decides is noise. What it decides is noise
includes an assistant talking out of your speakers — and, measured on this
desk, a good deal of the person trying to talk over it. With the camera's
suppression on, Familiar's microphone read **0.01** while the assistant spoke
and a raised voice only reached **0.30**; the same voice peaks around **0.6** in
a quiet room. That is the camera ducking, and it is why interrupting did not
work.

That behaviour is right in a meeting and wrong when you want to interrupt
something, which makes it a thing you switch rather than a thing you configure
once. Quick Settings is where GNOME puts things you switch.

**This is not part of Familiar.** It talks to a webcam, not to the assistant,
and either works without the other. It lives in this repository because this is
where the problem was found.

## What it needs

`link-ctl`, which speaks to the camera over its USB extension units:

```sh
uv tool install link-ctl      # or: pipx install link-ctl
```

Insta360 ships no Linux version of its Link Controller — their own
compatibility FAQ says so — and ALSA exposes nothing for this device but
`Mic Capture Switch` and `Mic Capture Volume`. Driving `link-ctl` is the only
way to reach the setting.

With `link-ctl` absent the toggle is present but insensitive and says why, which
is the honest state: the capability is missing, not broken.

## Caveats worth knowing

- **It changes the microphone for every application**, not just Familiar. Your
  meetings will carry more room noise with it off.
- `link-ctl` lists this command as verified on macOS, and Link 2 **Pro** support
  as unverified. The read and the write both work here; you are ahead of what
  the project has tested.
- The camera also does beamforming and has three audio modes (Voice Focus,
  Voice Suppression, Music Balance) which only the Windows/Mac client can set.
  Some processing remains whatever this switch says.
- Switching to a plain microphone entirely is the surer fix, and GNOME already
  does that well — the menu has a **Sound Settings** item pointing at it.

## Install

```sh
make install
gnome-extensions enable webcam-mic@hagreli.us
```

On Wayland the shell only discovers a newly installed extension at startup, so
log out and back in the first time.

## Development

```sh
make lint      # every file parses; a syntax error here takes the panel down
make logs      # follow this extension's shell output
make pack      # zip for extensions.gnome.org
```

## Layout

```
camera.js     the one external seam: link-ctl, as ok/err values
extension.js  the Quick Settings toggle and its menu
```

The switch shows what the **camera** reports, not what was last asked of it: a
write is followed by a read, because this is a device on the end of a USB cable
and not a variable. A camera that has been unplugged makes the toggle
insensitive with the reason, rather than latching while nothing happens.
