#!/bin/sh
if [ "$1" = "--version" ]; then echo 'gh version test'; exit 0; fi
case "$2" in
*check-runs*)
echo '{"total_count":2,"check_runs":[{"id":1,"name":"build","status":"completed","conclusion":"success"},{"id":2,"name":"lint","status":"completed","conclusion":"neutral"}]}'
;;
*)
echo '{"head":{"sha":"deadbeefsha"}}'
;;
esac
