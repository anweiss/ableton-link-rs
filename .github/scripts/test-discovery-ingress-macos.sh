#!/usr/bin/env bash
set -euo pipefail
# This fixture modifies networking only on a disposable GitHub Actions runner.
[[ "${GITHUB_ACTIONS:-}" == true && "$EUID" == 0 ]]
if [[ "${1:-}" == --churn ]]; then
  [[ "${LINK_154_ADAPTER_FIXTURE:-}" == 1 ]]
  ifconfig feth1540 inet 10.42.0.1 -alias
  ifconfig feth1540 inet 10.42.0.1/24 alias
  exit
fi
test -x "$1"
for name in feth1540 feth1541 feth1542 feth1543; do
  if ifconfig "$name" >/dev/null 2>&1; then
    echo "Refusing to replace existing interface $name" >&2
    exit 1
  fi
done
created=()
cleanup() {
  for name in "${created[@]}"; do ifconfig "$name" destroy; done
}
trap cleanup EXIT
for name in feth1540 feth1541 feth1542 feth1543; do
  ifconfig "$name" create
  created+=("$name")
done
ifconfig feth1540 peer feth1541
ifconfig feth1542 peer feth1543
for name in "${created[@]}"; do ifconfig "$name" up; done
ifconfig feth1540 inet 10.42.0.1/24 alias
ifconfig feth1541 inet 10.42.0.130/24 alias
ifconfig feth1540 inet 10.42.0.9/24 alias
ifconfig feth1542 inet 10.42.0.129/24 alias
ifconfig feth1543 inet 10.42.0.2/24 alias
ifconfig feth1542 inet 10.42.0.9/24 alias
export LINK_154_ADAPTER_FIXTURE=1
"$1" --ignored --exact discovery::messenger::tests::multihomed_adapter_ingress_and_churn --nocapture
