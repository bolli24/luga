#!/bin/sh
# Playground entrypoint, see CONTRACT.md in the playground-runner repo.
set -u

# Runners from before views don't set these
views=${PLAYGROUND_VIEWS:-}
run=${PLAYGROUND_RUN:-1}

# Build from the source root so diagnostics show paths relative to it
cd /work/src

luga build --out-dir /work/build ${views:+--emit "$views" --emit-dir /tmp/views} "$PLAYGROUND_ENTRY"
status=$?

# Views come after the compiler's output, also on a failed build, as far as it got. They can contain
# strings from the program, so \x1e is removed to keep them from faking a header or the marker.
for view in $(echo "$views" | tr ',' ' '); do
    [ -f "/tmp/views/$view.txt" ] || continue
    printf '\036PLAYGROUND-VIEW %s\036\n' "$view"
    tr -d '\036' < "/tmp/views/$view.txt" | head -c 262145
done

case $status in
    0) ;;
    3) exit 100 ;; # compile error
    *) exit 101 ;; # I/O, codegen, linker, anything unexpected
esac
[ "$run" = 1 ] || exit 0

# luga names the executable after the entry file, without extension
exe="/work/build/$(basename "${PLAYGROUND_ENTRY%.*}")"
[ -x "$exe" ] || { echo "playground: expected executable at $exe" >&2; exit 101; }

printf '\036PLAYGROUND-RUN\036\n'
printf '\036PLAYGROUND-RUN\036\n' >&2
exec "$exe" < /work/stdin
