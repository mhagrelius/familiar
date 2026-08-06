#!/usr/bin/env bash
#
# The voice that does not sound like 2005.
#
#   packaging/speech-server.sh          # install and start it
#   packaging/speech-server.sh stop     # stop it, leave it installed
#   packaging/speech-server.sh remove   # take it away entirely
#
# Familiar reads answers back through speech-dispatcher by default, because
# that is already on every GNOME desktop and needs nothing. It sounds like
# espeak, because it is espeak. This runs Kokoro instead — 82M parameters,
# Apache 2.0, and the current best quality per millisecond — behind an
# OpenAI-shaped /v1/audio/speech, which is what Familiar's "Speech server"
# voice speaks to.
#
# **CPU, not GPU.** The container image with CUDA in it is several times the
# size, and the GPU on this machine is already holding a 27B model — speech
# must not compete with the thing that does the answering. Kokoro on the CPU
# renders a sentence in about a quarter of a second, which is under the time
# the model takes to write the next one, so it is never the thing being waited
# for.
#
# A quadlet rather than `podman run`, so it comes back after a reboot without
# anything being remembered.
set -euo pipefail

IMAGE="ghcr.io/remsky/kokoro-fastapi-cpu:latest"
NAME="familiar-speech"
PORT=8880
UNIT_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/containers/systemd"
UNIT="$UNIT_DIR/$NAME.container"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

case "${1:-start}" in
  stop)
    say "Stopping"
    systemctl --user stop "$NAME" 2>/dev/null || podman stop "$NAME" 2>/dev/null || true
    exit 0
    ;;
  remove)
    say "Removing"
    systemctl --user stop "$NAME" 2>/dev/null || true
    podman rm -f "$NAME" 2>/dev/null || true
    rm -f "$UNIT"
    systemctl --user daemon-reload
    say "The image is still there. Remove it with: podman rmi $IMAGE"
    exit 0
    ;;
esac

command -v podman >/dev/null || {
  echo "podman is not installed." >&2
  exit 1
}

say "Fetching $IMAGE (about 3.4 GB, once)"
podman pull "$IMAGE"

# Bound to loopback on purpose. Nothing about this needs to be reachable from
# the network, and a speech server that is is a speech server somebody else can
# use.
mkdir -p "$UNIT_DIR"
cat > "$UNIT" <<EOF
[Unit]
Description=Kokoro speech synthesis for Familiar

[Container]
Image=$IMAGE
ContainerName=$NAME
PublishPort=127.0.0.1:$PORT:8880

[Service]
Restart=always

[Install]
WantedBy=default.target
EOF

say "Starting"
podman rm -f "$NAME" 2>/dev/null || true
systemctl --user daemon-reload
# `start`, never `enable`: the unit is generated from the quadlet, and
# systemd refuses to enable a generated unit. What makes it come back after a
# reboot is the [Install] section above, which the generator acts on.
systemctl --user start "$NAME.service"

say "Waiting for it to answer"
for _ in $(seq 1 60); do
  if curl -fsS -m 2 "http://127.0.0.1:$PORT/v1/audio/voices" >/dev/null 2>&1; then
    say "Ready on http://127.0.0.1:$PORT"
    echo
    say "In Familiar: Preferences → Voice → Read Answers Aloud → Speech server."
    say "The voice name is any of these; af_heart is the default:"
    curl -fsS "http://127.0.0.1:$PORT/v1/audio/voices" |
      sed 's/[{}]//g; s/"voices"://; s/"id":"/\n  /g; s/","name.*//' | grep -v '^\[' | head -20
    exit 0
  fi
  sleep 2
done

echo "It did not answer in two minutes. Look at: podman logs $NAME" >&2
exit 1
