@timeout 60000
@driver real-attach
@sandbox-config packages/cli/tests/fixtures/playbooks/history_boundary.toml
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
send-keys keys='printf "\\e[2J\\e[H"; seq 1 10000\r'
wait-for pattern='10000'
send-attach key='ctrl+a ['
sleep ms=1500
send-attach key='ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e'
sleep ms=300
assert-rendered contains='10000 '
assert-rendered contains='SCROLL'
send-attach key='ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e'
sleep ms=300
assert-rendered contains='10000 '
assert-rendered contains='SCROLL'
send-attach key='ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e'
sleep ms=300
assert-rendered contains='10000 '
assert-rendered contains='SCROLL'
send-attach key='ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e'
sleep ms=300
assert-rendered contains='10000 '
assert-rendered contains='SCROLL'
send-attach key='ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+y ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e ctrl+e'
sleep ms=300
assert-rendered contains='10000 '
assert-rendered contains='SCROLL'
send-attach key='escape'
