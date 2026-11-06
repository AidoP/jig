#!/bin/sh
# Shows what is still running once the tests have taken far longer than they should, as they have hung in CI at times,
# with a `jig` and a `rustc` left under the integration tests: each process, with its arguments, which name the
# project of the test, and on Linux, what each of jig's and rustc's waits for, its program, which is rustup's proxy
# while rustup installs a toolchain, and the variables which choose the toolchain and the jobserver.
echo "::group::processes"
ps -A -o pid,ppid,etime,stat,wchan,command
echo "::endgroup::"
[ -d /proc ] || exit 0
for dir in /proc/[0-9]*; do
    pid=${dir#/proc/}
    case $(cat "$dir/comm" 2>/dev/null) in
        jig | rustc | integration) ;;
        *) continue ;;
    esac
    echo "::group::$pid $(cat "$dir/comm" 2>/dev/null)"
    echo "program: $(sudo readlink "$dir/exe" 2>/dev/null)"
    tr '\0' ' ' < "$dir/cmdline" 2>/dev/null
    echo
    sudo cat "$dir/environ" 2>/dev/null | tr '\0' '\n' | grep -E '^(RUSTUP_|MAKEFLAGS|CARGO_MAKEFLAGS|HOME=)'
    sudo ls -l "$dir/fd" 2>/dev/null
    for task in "$dir"/task/*; do
        echo "thread ${task##*/}: $(cat "$task/comm" 2>/dev/null) $(cat "$task/wchan" 2>/dev/null)"
        sudo cat "$task/stack" 2>/dev/null
    done
    if command -v gdb >/dev/null; then
        sudo gdb -p "$pid" -batch -ex 'thread apply all bt' 2>/dev/null | grep -E '^(Thread|#)'
    fi
    echo "::endgroup::"
done
