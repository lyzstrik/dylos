#!/bin/sh
set -eu
exec "$@" --log-path /run/freeze.log
