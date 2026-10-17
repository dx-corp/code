#!/bin/sh
# Each test symlinks this immutable executable into its isolated workspace.
count_path="$(dirname "$0")/check-runs.count"
if [ "$1" = "--version" ]; then echo 'gh version test'; exit 0; fi
if [ "$1" = "run" ]; then
  for i in $(seq 1 300); do echo "log line $i"; done
  exit 0
fi
case "$2" in
  *check-runs*)
    count=$(cat "$count_path" 2>/dev/null || echo 0)
    count=$((count+1))
    echo "$count" > "$count_path"
    if [ "$count" -lt 3 ]; then
      echo '{"total_count":1,"check_runs":[{"id":1,"name":"build","status":"in_progress","conclusion":null,"details_url":"https://github.com/acme/widget/actions/runs/111/job/222"}]}'
    else
      echo '{"total_count":1,"check_runs":[{"id":1,"name":"build","status":"completed","conclusion":"failure","details_url":"https://github.com/acme/widget/actions/runs/111/job/222"}]}'
    fi
    ;;
  *)
    echo '{"head":{"sha":"deadbeefsha"}}'
    ;;
esac
