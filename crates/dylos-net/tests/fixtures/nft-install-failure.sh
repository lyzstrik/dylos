#!/bin/sh
IFS= read -r operation
case "$operation" in
    table*) exit 1 ;;
    'destroy table bridge dylos_peer_filter') exit 0 ;;
    *) exit 2 ;;
esac
