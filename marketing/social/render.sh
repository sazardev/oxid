#!/usr/bin/env bash
# Renders every marketing graphic to PNG via headless Chrome.
# Usage: ./render.sh
set -euo pipefail
cd "$(dirname "$0")"

CHROME=google-chrome-stable

render() {
  local layout="$1" out="$2" w="$3" h="$4"
  "$CHROME" --headless --disable-gpu --hide-scrollbars \
    --force-device-scale-factor=1 \
    --run-all-compositor-stages-before-draw \
    --virtual-time-budget=4000 \
    --window-size="${w},${h}" \
    --screenshot="$(pwd)/${out}" \
    "file://$(pwd)/${layout}" >/dev/null 2>&1
  echo "  ${out} (${w}x${h})"
}

echo "Rendering Oxid marketing graphics..."
render layout-card.html    og-image.png               1200 630
render layout-card.html    facebook-post.png          1200 630
render layout-card.html    twitter-card.png           1600 900
render layout-card.html    github-social-preview.png  1280 640
render layout-wide.html    linkedin-banner.png        1584 396
render layout-wide.html    facebook-cover.png          820 312
render layout-tall.html    linkedin-post.png          1200 1200
render layout-tall.html    instagram-square.png       1200 1200
render layout-tall.html    instagram-portrait.png     1080 1350
render layout-hero.html    hero-landscape.png         1920 1080
echo "Done."
