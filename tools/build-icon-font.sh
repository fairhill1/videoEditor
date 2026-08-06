#!/usr/bin/env bash
# Rebuild assets/fonts/lucide-subset.ttf from the upstream Lucide icon font.
#
# The full font carries ~2000 glyphs at 848KB; we use a handful, so it gets
# subset down to a few KB. The subset is committed, so this only needs running
# when the icon set below changes — add the icon's name to ICONS and re-run.
#
# Needs: curl, python3 with fonttools (pip install fonttools).

set -euo pipefail

ICONS=(
    play                      # transport: play
    pause                     # transport: pause
    chevron-left              # transport: previous frame
    chevron-right             # transport: next frame
    chevron-first             # transport: previous clip edge
    chevron-last              # transport: next clip edge
    square-split-horizontal   # timeline: split at playhead
    trash                     # timeline: delete selected clip
    undo                      # timeline: undo
    redo                      # timeline: redo
    magnet                    # timeline: snap toggle
    film                      # timeline: render to file
    square                    # timeline: stop a running render
    folder-open               # media pool: open files
    x                         # media pool: remove a row
)

repo_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
out="$repo_root/assets/fonts/lucide-subset.ttf"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "Fetching lucide-static..."
tarball=$(curl -fsSL https://registry.npmjs.org/lucide-static/latest \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["dist"]["tarball"])')
curl -fsSL "$tarball" | tar xz -C "$work"

# codepoints.json maps icon names to the private-use codepoints the font uses.
codepoints=$(ICONS="${ICONS[*]}" python3 - "$work/package/font/codepoints.json" <<'PY'
import json, os, sys
table = json.load(open(sys.argv[1]))
names = os.environ["ICONS"].split()
missing = [n for n in names if n not in table]
if missing:
    sys.exit("not in this Lucide release: " + ", ".join(missing))
print(",".join("U+%04X" % table[n] for n in names))
PY
)
echo "Subsetting ${#ICONS[@]} icons: $codepoints"

python3 -m fontTools.subset "$work/package/font/lucide.ttf" \
    --unicodes="$codepoints" \
    --output-file="$out" \
    --no-hinting --desubroutinize --layout-features='' \
    --drop-tables+=DSIG --name-IDs='*'

echo "Wrote $out ($(wc -c <"$out") bytes)"
echo "Codepoints are mirrored in src/text.rs — update it if the set changed."
