@timeout 30000
@driver real-attach
@sandbox-config packages/cli/tests/fixtures/playbooks/history_boundary.toml
@viewport cols=80 rows=24
@shell sh
@env-mode clean

new-session
send-keys keys='printf "\\e[2J\\e[H"; seq 1 30\r'
wait-for pattern='30'
send-attach key='ctrl+a ['
sleep ms=500
assert-rendered contains='SCROLL'
send-attach key='g'
sleep ms=500
assert-rendered contains='│1 '
resize-viewport cols=40 rows=30
sleep ms=500
send-attach key='g'
sleep ms=500
assert-rendered contains='│1 '
assert-rendered contains='│27 '
send-attach key='g'
sleep ms=500
assert-rendered contains='│1 '
send-attach key='escape'
