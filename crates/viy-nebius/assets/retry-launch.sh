# Run through pinned SSH only after the legacy workspace probe failed.
# Keep stdin for gpu-launch's original JSON payload.
set -eu
helper=/usr/local/sbin/gpu-workspace
sudo -n grep -Fxq '    signatures=$(wipefs --noheadings -o TYPE "$dev")' "$helper"
sudo -n sed -i '/^    signatures=$(wipefs --noheadings -o TYPE "[$]dev")$/s/--noheadings -o/--no-act --noheadings --output/' "$helper"
