#!/bin/sh
# Playground entrypoint, see CONTRACT.md in the playground-runner repo.
set -u

# Build from the source root so diagnostics show paths relative to it
cd /work/src

luga build --out-dir /work/build "$PLAYGROUND_ENTRY"
case $? in
    0) ;;
    3) exit 100 ;; # compile error
    *) exit 101 ;; # I/O, codegen, linker, anything unexpected
esac

# luga names the executable after the entry file, without extension
exe="/work/build/$(basename "${PLAYGROUND_ENTRY%.*}")"
[ -x "$exe" ] || { echo "playground: expected executable at $exe" >&2; exit 101; }

printf '\036PLAYGROUND-RUN\036\n'
printf '\036PLAYGROUND-RUN\036\n' >&2
exec "$exe" < /work/stdin
