#!/usr/bin/env bash
# Build the image on ubuntu02, push it to the registry on ubuntu04, roll out.
#   ./deploy/deploy.sh [tag]
set -euo pipefail

TAG="${1:-$(date +%Y%m%d-%H%M)}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="localhost:5000/thermal-monitor:$TAG"
TAR="/tmp/thermal-monitor-$TAG.tar"
SSH="ssh -o BatchMode=yes"

rsync -a --exclude target --exclude .git --exclude deploy "$ROOT/" ubuntu02:~/thermal-monitor/
$SSH ubuntu02 "cd ~/thermal-monitor && docker build -q -t thermal-monitor:$TAG . && docker save thermal-monitor:$TAG > $TAR"
scp -q -o BatchMode=yes "ubuntu02:$TAR" "$TAR"

$SSH -f -N -L 15000:127.0.0.1:5000 ubuntu04
trap 'pkill -f "L 15000:127.0.0.1:5000" 2>/dev/null || true' EXIT
sleep 1
crane push "$TAR" "localhost:15000/thermal-monitor:$TAG" >/dev/null
pkill -f "L 15000:127.0.0.1:5000" 2>/dev/null || true

$SSH ubuntu03 "kubectl apply -f -" < "$ROOT/deploy/k8s.yaml"
$SSH ubuntu03 "kubectl -n thermal-monitor set image deploy/thermal-monitor app=$IMAGE && kubectl -n thermal-monitor rollout status deploy/thermal-monitor --timeout=120s"

rm -f "$TAR"
$SSH ubuntu02 "rm -f $TAR"
echo "deployed $IMAGE"
