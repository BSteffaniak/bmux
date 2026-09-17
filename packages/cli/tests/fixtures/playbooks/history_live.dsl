@timeout 30000
@driver real-attach
@sandbox-config packages/cli/tests/fixtures/playbooks/history_live.toml
@viewport cols=80 rows=24
@shell sh
@env-mode clean

new-session
send-keys keys='printf "\\e[2J\\e[H"; seq 1 60\r'
wait-for pattern='60'
send-attach key='ctrl+a ['
sleep ms=300
assert-rendered contains='SCROLL'
send-attach key='g'
sleep ms=500
assert-rendered contains='│1 '
send-keys keys='seq 61 120\r'
sleep ms=500
assert-rendered contains='│1 '
resize-viewport cols=40 rows=30
send-attach key='g'
sleep ms=500
assert-rendered contains='SCROLL'
send-attach key='escape'
send-attach key='ctrl+a ['
send-attach key='g'
sleep ms=500
assert-rendered contains='│1 '
send-attach key='escape'
