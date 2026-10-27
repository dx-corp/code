#!/bin/sh
if [ "$1" = "--version" ]; then echo 'gh version test'; exit 0; fi
state_dir=${0%/*}
printf '%s' "$$" > "$state_dir/api.pid"
sleep 60
printf completed > "$state_dir/completed"
