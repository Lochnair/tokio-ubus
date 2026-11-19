#!/bin/sh
# The in-container half of run.sh.
set -eu
sock=/tmp/ubus.sock
failures=0

pass() { echo "ok   - $1"; }
fail() { echo "FAIL - $1"; failures=$((failures + 1)); }
# check <name> <jq filter> <command...>: the command succeeds and its JSON
# output satisfies the filter.
check() {
    name=$1 filter=$2; shift 2
    if out=$("$@" 2>&1) && printf '%s' "$out" | jq -e "$filter" >/dev/null 2>&1; then
        pass "$name"
    else
        fail "$name: $out"
    fi
}
# status <name> <expected exit code> <command...>
status() {
    name=$1 want=$2; shift 2
    set +e; out=$("$@" 2>&1); got=$?; set -e
    if [ "$got" = "$want" ]; then pass "$name"; else fail "$name: exit $got, want $want: $out"; fi
}
u() { ubus -s "$sock" "$@"; }

cargo build -q --example demo
ubusd -s "$sock" & ubusd_pid=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -S "$sock" ] && break; sleep 0.1; done
target/debug/examples/demo "$sock" 2>/tmp/demo.log & demo_pid=$!
trap 'kill $demo_pid $ubusd_pid 2>/dev/null || true' EXIT
u -t 5 wait_for demo demo.second

out=$(u -v list demo 2>&1)
case $out in
    *'"echo":{"message":"String","count":"Integer"}'*'"sleep":{"ms":"Integer"}'*) pass "signature lists methods and argument types" ;;
    *) fail "signature: $out" ;;
esac
check "status returns every value type" \
    '.calls == 1 and .version == "0.1.0" and .dry_run == false and .ratio == 0.25 and .small == -7 and .unset == null and .download == {"interface":"ifb4wan","rate_kbps":41200} and .reflectors == ["1.1.1.1","9.9.9.9"]' \
    u call demo status
check "arguments arrive intact, with the caller's identity" \
    '.message == "hi" and .count == 3 and .nested.deep == [1, {"x": true}] and .user == "root"' \
    u call demo echo '{"message":"hi","count":3,"nested":{"deep":[1,{"x":true}]}}'
check "second object on the same connection" '.pong == true' u call demo.second ping
check "512 KiB reply" '.blob | length == 524288' u call demo big
# The CLI exits with the negated status, so 256 - status.
status "handler error becomes the exit status" 250 u call demo deny
status "unknown method" 253 u call demo missing
# json-c and blobmsg_format_json pass non-UTF-8 bytes through untouched.
latin1=$(printf 'caf\351')
out=$(u call demo echo "{\"message\":\"$latin1\"}" 2>&1)
if printf '%s' "$out" | LC_ALL=C grep -q "\"$latin1\""; then
    pass "non-UTF-8 string bytes round-trip"
else
    fail "non-UTF-8 string: $(printf '%s' "$out" | od -c | head -3)"
fi
status "no data reply" 0 u call demo nothing
[ -z "$(u call demo nothing)" ] && pass "no data reply prints nothing" || fail "no data reply printed output"

start=$(date +%s%N)
pids=
for _ in 1 2 3 4 5; do u call demo sleep '{"ms":1000}' >/dev/null & pids="$pids $!"; done
wait_failed=0
for pid in $pids; do wait "$pid" || wait_failed=1; done
elapsed=$(( ($(date +%s%N) - start) / 1000000 ))
if [ "$wait_failed" = 0 ] && [ "$elapsed" -lt 2500 ]; then
    pass "5 concurrent 1s calls took ${elapsed}ms"
else
    fail "concurrent calls took ${elapsed}ms (failed=$wait_failed)"
fi

# A second provider whose "demo" clashes with the first keeps its other object.
target/debug/examples/demo "$sock" demo.third 2>/tmp/demo2.log & demo2_pid=$!
if u -t 5 wait_for demo.third; then pass "refused object spares the others"; else fail "demo.third never registered: $(cat /tmp/demo2.log)"; fi
check "the spared object answers" '.pong == true' u call demo.third ping
grep -q 'Registering ubus object "demo" failed' /tmp/demo2.log \
    && pass "the refusal is logged" || fail "no refusal logged: $(cat /tmp/demo2.log)"
check "the first provider keeps its objects" '.pong == true' u call demo.second ping
kill "$demo2_pid"; wait "$demo2_pid" 2>/dev/null || true

kill "$ubusd_pid"; wait "$ubusd_pid" 2>/dev/null || true
rm -f "$sock"
sleep 0.5
ubusd -s "$sock" & ubusd_pid=$!
# wait_for fails outright until ubusd is listening.
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -S "$sock" ] && break; sleep 0.1; done
if u -t 10 wait_for demo demo.second; then pass "re-registered after ubusd restart"; else fail "no re-registration"; fi
check "calls work after the restart" '.calls == 2' u call demo status

kill "$demo_pid"; wait "$demo_pid" 2>/dev/null || true
# A failed lookup exits with the plain status.
status "objects go away with the process" 4 u call demo status

echo "--- demo log"; cat /tmp/demo.log
[ "$failures" = 0 ] && echo "all checks passed" || { echo "$failures check(s) failed"; exit 1; }
