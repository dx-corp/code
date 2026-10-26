#!/bin/sh
if [ "$1" = "--version" ]; then echo 'gh version test'; exit 0; fi
case "$2" in
*check-runs*)
echo '{"total_count":1,"check_runs":[{"id":1,"name":"build","status":"in_progress","conclusion":null,"details_url":null}]}'
;;
*)
echo '{"head":{"sha":"deadbeefsha"}}'
;;
esac
