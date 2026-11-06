#!/bin/sh
printf '%s\n' "$@"
exit "${E2E_TEST_EXIT_CODE:-0}"
