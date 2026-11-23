#!/bin/sh
# Test double for ssh: runs the "remote" command on this machine.
for a in "$@"; do
  case "$a" in
    -O) exit 1 ;;      # `ssh -O check`: no multiplex master
    -fN) exit 0 ;;     # master start: pretend it worked
  esac
done
for a in "$@"; do last="$a"; done
exec sh -c "$last"
