@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='session-first'
send-keys keys='printf "\\033[2J\\033[HSESSION_FIRST_MARKER\\n"\r'
wait-for pattern='SESSION_FIRST_MARKER'
send-attach key='ctrl+a shift+c'
sleep ms=700
send-keys keys='printf "\\033[2J\\033[HSESSION_SECOND_MARKER\\n"\r'
wait-for pattern='SESSION_SECOND_MARKER'
assert-screen contains='SESSION_SECOND_MARKER'
assert-screen not_contains='SESSION_FIRST_MARKER'
