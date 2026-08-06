#!/usr/bin/env bash
#
# Fetch the two speech models Familiar listens with.
#
#   packaging/fetch-speech-models.sh            # both
#   packaging/fetch-speech-models.sh accurate   # just the one that is sent
#
# About 700 MB into ~/.local/share/familiar/models. Nothing here is required to
# run Familiar — everything except listening works without a model, and the
# Voice page in Preferences says so rather than failing at the microphone.
#
# If Scribe is installed it has already downloaded these and Familiar reads its
# copy, so there is nothing to do. This script is for a machine without it.
#
# **Accurate** is Parakeet TDT 0.6B v3, which transcribes the utterance that
# gets sent. **Live** is Nemotron's streaming encoder, which puts words on
# screen while they are being said and is thrown away afterwards. Either alone
# works: with only Accurate there is no preview, and with only Live the preview
# is what gets sent.
set -euo pipefail

DIR="${XDG_DATA_HOME:-$HOME/.local/share}/familiar/models"
WHICH="${1:-both}"

ACCURATE_BASE="https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx/resolve/main"
ACCURATE_FILES=(
  encoder-model.int8.onnx
  decoder_joint-model.int8.onnx
  nemo128.onnx
  vocab.txt
  config.json
)

LIVE_BASE="https://huggingface.co/lokkju/nemotron-speech-streaming-en-0.6b-int8/resolve/main"
LIVE_FILES=(
  encoder.onnx
  decoder_joint.onnx
  tokenizer.model
)

fetch() {
  local base="$1" into="$2"
  shift 2
  mkdir -p "$into"
  for file in "$@"; do
    if [ -s "$into/$file" ]; then
      echo "have $file"
      continue
    fi
    echo "fetching $file"
    # A partial download must not look like a finished one: write beside the
    # target and move it into place only once curl is happy.
    curl -fL --progress-bar "$base/$file" -o "$into/$file.part"
    mv "$into/$file.part" "$into/$file"
  done
}

if [ "$WHICH" = both ] || [ "$WHICH" = accurate ]; then
  fetch "$ACCURATE_BASE" "$DIR/parakeet-tdt-0.6b-v3-int8" "${ACCURATE_FILES[@]}"
fi
if [ "$WHICH" = both ] || [ "$WHICH" = live ]; then
  fetch "$LIVE_BASE" "$DIR/nemotron-streaming-en-0.6b" "${LIVE_FILES[@]}"
fi

echo
echo "Done. $DIR"
echo "Check it with: cargo run --release --example hear -- some.wav"
