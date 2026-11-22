#!/bin/sh
# Pull-based deploy (no server keys in GitHub), same scheme as the other projects on the VPS: every 2 minutes a
# systemd timer runs this; when origin/main is not the last commit that deployed successfully, it pulls and
# rebuilds, so a failed build is retried on the next run. Touches only this project's compose stack.
set -eu
cd "$(dirname "$0")/.."
git fetch -q origin main
target="$(git rev-parse origin/main)"
[ "$(cat deploy/.deployed 2>/dev/null)" = "$target" ] && exit 0
git merge -q --ff-only origin/main
docker compose up -d --build --remove-orphans
echo "$target" > deploy/.deployed
docker image prune -f >/dev/null
echo "deployed $(git rev-parse --short HEAD)"
