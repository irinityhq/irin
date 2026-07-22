#!/usr/bin/env bash
# Rebuild the README walkthrough, hero GIF, and stills from one raw screen capture.
set -euo pipefail

if [[ $# -lt 1 || $# -gt 2 ]]; then
  printf 'usage: %s RAW_CAPTURE [OUTPUT_DIR]\n' "$0" >&2
  exit 2
fi

raw_capture="$1"
output_dir="${2:-assets/readme}"

if [[ ! -f "$raw_capture" ]]; then
  printf 'FAIL: raw capture not found: %s\n' "$raw_capture" >&2
  exit 2
fi

for dependency in ffmpeg ffprobe; do
  if ! command -v "$dependency" >/dev/null 2>&1; then
    printf 'FAIL: %s is required\n' "$dependency" >&2
    exit 2
  fi
done

duration="$(ffprobe -v error -show_entries format=duration \
  -of default=noprint_wrappers=1:nokey=1 "$raw_capture")"
if ! awk -v duration="$duration" 'BEGIN { exit !(duration >= 60) }'; then
  printf 'FAIL: raw capture must be at least 60 seconds (found %ss)\n' "$duration" >&2
  exit 2
fi

mkdir -p "$output_dir"
palette_file="$(mktemp /private/tmp/irin-readme-palette.XXXXXX.png)"
trap 'rm -f "$palette_file"' EXIT

# The raw macOS capture is 1920x1200. Remove the 32px browser-control strip,
# retain the full War Room layout, and produce a broadly playable H.264 file.
ffmpeg -y -i "$raw_capture" \
  -vf 'crop=1920:1168:0:32,scale=1280:-2,fps=30' \
  -an -c:v libx264 -preset slow -crf 27 -pix_fmt yuv420p \
  -movflags +faststart "$output_dir/warroom-walkthrough.mp4"

# The 12-second loop moves from round two into Sheldon's evidence panel.
ffmpeg -y -ss 30 -t 12 -i "$raw_capture" \
  -vf 'crop=1920:1168:0:32,fps=10,scale=960:-2:flags=lanczos,palettegen=max_colors=128:stats_mode=diff' \
  "$palette_file"
ffmpeg -y -ss 30 -t 12 -i "$raw_capture" -i "$palette_file" \
  -lavfi '[0:v]crop=1920:1168:0:32,fps=10,scale=960:-2:flags=lanczos[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=3:diff_mode=rectangle' \
  -loop 0 "$output_dir/warroom-deliberation.gif"

while IFS=: read -r timestamp name; do
  ffmpeg -y -ss "$timestamp" -i "$raw_capture" \
    -vf 'crop=1920:1168:0:32,scale=1280:-2' -frames:v 1 \
    "$output_dir/$name.png"
done <<'FRAMES'
18:seat-stream
42:sheldon-validation
58:chair-ruling
FRAMES

printf 'README media rebuilt in %s\n' "$output_dir"
