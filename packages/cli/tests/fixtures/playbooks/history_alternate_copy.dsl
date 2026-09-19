@timeout 30000
@driver real-attach
@sandbox-config packages/cli/tests/fixtures/playbooks/history_boundary.toml
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
send-keys keys='printf "MAIN-CONTENT\\n\\e[?1049h\\e[2J\\e[HALTERNATE-COPY"\r'
wait-for pattern='ALTERNATE-COPY'
send-attach key='ctrl+a ['
sleep ms=300
assert-rendered contains='ALTERNATE-COPY'
assert-rendered contains='SCROLL'
send-attach key='g'
sleep ms=100
assert-rendered contains='ALTERNATE-COPY'
resize-viewport cols=60 rows=20
sleep ms=300
assert-rendered contains='ALTERNATE-COPY'
send-attach key='escape'
send-keys keys='printf "\\e[?1049l"\r'
