@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='bootstrap'
send-attach key='c'
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HTAB_ONE_MARKER\\n"\r'
wait-for pattern='TAB_ONE_MARKER'
send-attach key='c'
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HTAB_TWO_MARKER\\n"\r'
wait-for pattern='TAB_TWO_MARKER'
send-attach key='n'
sleep ms=500
send-keys keys='printf "\\033[2J\\033[HNAV_RESULT_MARKER\\n"\r'
wait-for pattern='NAV_RESULT_MARKER'
send-attach key='p'
sleep ms=500
assert-screen contains='TAB_TWO_MARKER'
send-attach key='n'
sleep ms=500
assert-screen contains='NAV_RESULT_MARKER'
send-attach key='alt+f'
sleep ms=300
assert-rendered contains='default/tab-2'
send-attach key='t'
send-attach key='a'
send-attach key='b'
send-attach key='-'
send-attach key='2'
sleep ms=200
assert-rendered contains='default/tab-2'
resize-viewport cols=60 rows=16
sleep ms=300
assert-rendered contains='default/tab-2'
resize-viewport cols=120 rows=36
sleep ms=300
assert-rendered contains='default/tab-2'
send-attach key='enter'
sleep ms=500
assert-screen contains='TAB_TWO_MARKER'
