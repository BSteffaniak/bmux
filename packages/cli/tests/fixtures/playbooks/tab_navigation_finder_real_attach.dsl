@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='bootstrap'
send-attach key='c'
wait-rendered contains='<active:tab-1>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HTAB_ONE_MARKER\\n"\r'
wait-for pattern='TAB_ONE_MARKER'
send-attach key='c'
wait-rendered contains='<active:tab-2>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HTAB_TWO_MARKER\\n"\r'
wait-for pattern='TAB_TWO_MARKER'
# Establish a named third target, then exercise actual previous/next navigation.
send-attach key='alt+v'
wait-rendered contains='<active:nav-target>' timeout=10000
send-attach key='p'
wait-rendered contains='<active:tab-2>' timeout=10000
wait-for pattern='TAB_TWO_MARKER' timeout=10000
send-attach key='n'
wait-rendered contains='<active:nav-target>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HNAV_RESULT_MARKER\\n"\r'
wait-for pattern='NAV_RESULT_MARKER'
send-attach key='p'
wait-rendered contains='<active:tab-2>' timeout=10000
wait-for pattern='TAB_TWO_MARKER' timeout=10000
assert-screen contains='TAB_TWO_MARKER'
send-attach key='n'
wait-rendered contains='<active:nav-target>' timeout=10000
wait-for pattern='NAV_RESULT_MARKER' timeout=10000
assert-screen contains='NAV_RESULT_MARKER'
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=10000
assert-rendered contains='default/tab-2'
send-attach key='t'
send-attach key='a'
send-attach key='b'
send-attach key='-'
send-attach key='2'
wait-rendered contains='default/tab-2' timeout=10000
assert-rendered contains='default/tab-2'
resize-viewport cols=60 rows=16
wait-rendered contains='default/tab-2' timeout=10000
assert-rendered contains='default/tab-2'
resize-viewport cols=120 rows=36
wait-rendered contains='default/tab-2' timeout=10000
assert-rendered contains='default/tab-2'
send-attach key='enter'
wait-rendered contains='<active:tab-2>' timeout=10000
wait-for pattern='TAB_TWO_MARKER' timeout=10000
assert-screen contains='TAB_TWO_MARKER'
