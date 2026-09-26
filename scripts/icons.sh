#!/bin/sh
# Regenerates the app icon and the menu bar icons from assets/. Needs ImageMagick.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/fastflow_ui_macos/bundle"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# App icon: drop the near-transparent specks outside the tile, then fit the tile to Apple's
# grid of an 824px tile on a 1024px canvas.
src="$root/assets/app_icon.png"
bbox=$(magick "$src" -alpha extract -threshold 4% -format "%@" info:)
size=$(magick "$src" -format "%wx%h" info:)
magick "$src" -alpha extract -resize 25% -threshold 4% -morphology Open Disk:1 \
  -morphology Dilate Disk:4 -resize "$size!" -blur 0x4 "$work/mask.png"
magick "$src" "$work/mask.png" -compose CopyOpacity -composite \
  -compose Over -crop "$bbox" +repage -resize 824x824 \
  -background none -gravity center -extent 1024x1024 "$work/app.png"

set_dir="$work/fastflow.iconset"
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
  magick "$work/app.png" -resize ${s}x${s} "$set_dir/icon_${s}x${s}.png"
  magick "$work/app.png" -resize $((s * 2))x$((s * 2)) "$set_dir/icon_${s}x${s}@2x.png"
done
iconutil -c icns "$set_dir" -o "$out/fastflow.icns"

# Menu bar: 18pt tall at 2x. The recording variant fills the screen and knocks the chevrons out.
src="$root/assets/menu_icon.png"
magick "$src" -alpha extract -threshold 4% -trim +repage -resize x200 -threshold 50% \
  -bordercolor black -border 20 "$work/glyph.png"
h=$(magick "$work/glyph.png" -format "%h" info:)
magick "$work/glyph.png" -fill white -draw "color 60,$((h / 2)) floodfill" "$work/solid.png"
magick "$work/solid.png" -morphology Erode Disk:14 "$work/inner.png"
magick "$work/glyph.png" "$work/inner.png" -compose Multiply -composite \
  -morphology Dilate Disk:5 "$work/chevrons.png"
magick "$work/solid.png" "$work/chevrons.png" -compose MinusSrc -composite "$work/filled.png"

tray() {
  magick "$1" -morphology Dilate Disk:2 -trim +repage -resize x26 "$work/small.png"
  sw=$(magick "$work/small.png" -format "%w" info:)
  magick "$work/small.png" -background black -gravity center -extent "$((sw + 4))x36" \
    \( +clone -fill black -colorize 100 \) +swap -compose CopyOpacity -composite \
    -define png:color-type=6 "$out/$2"
}
tray "$work/glyph.png" tray.png
tray "$work/filled.png" tray_recording.png
