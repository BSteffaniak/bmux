@timeout 60000
@driver real-attach
@sandbox-config packages/cli/tests/fixtures/playbooks/history_boundary.toml
@viewport cols=80 rows=24
@shell sh
@env-mode clean
@var LINES=1000
new-session
send-keys keys='printf "\\e[2J\\e[H"; seq 1 ${LINES}\r'
wait-for pattern='${LINES}'
send-attach key='ctrl+a ['
sleep ms=500
send-attach key='g'
sleep ms=1000
assert-rendered contains='│1 '
send-attach key='shift+g'
sleep ms=500
assert-rendered contains='${LINES} '
send-attach key='ctrl+y'
send-attach key='ctrl+y'
send-attach key='ctrl+e'
send-attach key='ctrl+e'
sleep ms=500
assert-rendered contains='${LINES} '
resize-viewport cols=40 rows=30
sleep ms=500
send-attach key='g'
sleep ms=1000
assert-rendered contains='│1 '
send-attach key='shift+g'
sleep ms=500
assert-rendered contains='${LINES} '
send-attach key='escape'
