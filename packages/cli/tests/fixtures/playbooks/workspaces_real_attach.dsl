@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='workspace-first'
send-attach key='c'
sleep ms=500
send-keys keys='printf "\\033[2J\\033[HWORKSPACE_FIRST_MARKER\\n"\r'
wait-for pattern='WORKSPACE_FIRST_MARKER'
send-attach key='alt+w'
sleep ms=700
assert-screen not_contains='WORKSPACE_FIRST_MARKER'
send-keys keys='printf "\\033[2J\\033[HWORKSPACE_SECOND_MARKER\\n"\r'
wait-for pattern='WORKSPACE_SECOND_MARKER'
send-attach key='c'
sleep ms=700
assert-screen not_contains='WORKSPACE_SECOND_MARKER'
send-attach key='alt+1'
sleep ms=700
assert-screen contains='WORKSPACE_SECOND_MARKER'
send-attach key='alt+l'
sleep ms=700
assert-screen contains='WORKSPACE_FIRST_MARKER'
send-attach key='alt+l'
sleep ms=700
assert-screen contains='WORKSPACE_SECOND_MARKER'
