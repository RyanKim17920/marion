S=<SCRATCH>
EV=<HOME>/Desktop/CODING/mw-review/tests/fixtures/s38-read-only
export DISABLE_AUTOUPDATER=1 OPENCODE_DISABLE_AUTOUPDATE=1 QWEN_CODE_SKIP_UPDATE_CHECK_ONCE=true PI_SKIP_VERSION_CHECK=1 CLINE_NO_AUTO_UPDATE=1 COPILOT_AUTO_UPDATE=false AGY_CLI_DISABLE_AUTO_UPDATE=true
# start_canned <port> <reqlog> ; script from $S38_SCRIPT
start_canned() {
  rm -f "$2"
  MARION_CANNED_PORT=$1 MARION_CANNED_REQLOG=$2 $S/canned/target/debug/s38-canned 2>$S/canned-$1.log &
  CANNED_PID=$!
  for i in $(seq 50); do nc -z 127.0.0.1 $1 2>/dev/null && return 0; sleep 0.1; done
  echo "canned did not bind"; return 1
}
stop_canned() { kill $CANNED_PID 2>/dev/null; wait $CANNED_PID 2>/dev/null; }
# fresh git work dir
fresh_work() { rm -rf "$1"; mkdir -p "$1/src"; (cd "$1" && git init -q && echo hi > README && git add . && git -c user.email=a@b -c user.name=t commit -qm init); }
# run_case <harness> <label> <port> <workdir> <cmd...> ; runs in workdir, stdin closed
run_case() {
  local h=$1 l=$2 port=$3 w=$4; shift 4
  mkdir -p $EV/$h
  start_canned $port $S/$h-$l.req.jsonl || return 1
  (cd $w && timeout ${TMO:-90} "$@" </dev/null > $EV/$h/$l.stdout.txt 2> $EV/$h/$l.stderr.txt; echo "exit=$?" > $EV/$h/$l.exit.txt)
  stop_canned
  { echo "argv: $*"; echo "env names: ${ENVNAMES:-}"; } > $EV/$h/$l.argv.txt
  python3 $S/tools.py $S/$h-$l.req.jsonl > $EV/$h/$l.provider.txt
  local f=$(cat $w/src/ro.txt 2>/dev/null)
  echo "file src/ro.txt: ${f:-ABSENT}" >> $EV/$h/$l.exit.txt
  echo "== $h/$l: $(tr '\n' ' ' < $EV/$h/$l.exit.txt) | $(tr '\n' ' ' < $EV/$h/$l.provider.txt)"
}
