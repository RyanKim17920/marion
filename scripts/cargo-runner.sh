#!/bin/sh
# cargo's runner (.cargo/config.toml): execs the binary cargo built, with its arguments.
#
# The one change: the it_live test binary runs at most 3 tests at a time. Each of its tests drives
# real harness processes, and at a thread per core a full run peaked at 6-8 GB and a load of 150
# on 12 cores, where opencode and pi boots overran their bounds (tests/common/boot.rs). A caller
# who sets RUST_TEST_THREADS or passes --test-threads keeps their own choice.

case "${1##*/}" in
it_live-*)
	if [ -z "${RUST_TEST_THREADS-}" ]; then
		for arg in "$@"; do
			case $arg in
			--test-threads | --test-threads=*) exec "$@" ;;
			esac
		done
		exec "$@" --test-threads=3
	fi
	;;
esac
exec "$@"
