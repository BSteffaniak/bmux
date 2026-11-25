@driver real-attach
@timeout 90000
@viewport cols=120 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/personal_view_remove_real_attach.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:alpha>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HALPHA_READY\\n"\r'
wait-for pattern='ALPHA_READY' timeout=10000
send-attach key='alt+b'
wait-rendered contains='<active:beta>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HBETA_READY\\n"\r'
wait-for pattern='BETA_READY' timeout=10000
# Remove beta from this attachment's view: it moves to the previous member,
# alpha, and the bar offers to undo. Beta keeps running.
send-attach key='alt+x'
wait-rendered contains='<active:alpha>' timeout=10000
wait-rendered contains='Removed beta' timeout=10000
# Undo puts beta back in its place and selects it again.
send-attach key='alt+u'
wait-rendered contains='<active:beta>' timeout=10000
assert-rendered matches='<[a-z]+:alpha>[^<]*<active:beta>'
assert-screen contains='BETA_READY'
# Empty the view: remove beta, then alpha. The attachment stays connected
# with no input target and the bar says how to add tabs.
send-attach key='alt+x'
wait-rendered contains='<active:alpha>' timeout=10000
send-attach key='alt+x'
wait-rendered contains='View empty' timeout=10000
# Typing now reaches no tab: not alpha, which was shown last.
send-attach key='7'
send-attach key='7'
send-attach key='7'
send-attach key='7'
send-attach key='enter'
sleep ms=1000
# Undo restores alpha and resumes input there; nothing typed while the view
# was empty arrived.
send-attach key='alt+u'
wait-rendered contains='<active:alpha>' timeout=10000
assert-screen contains='ALPHA_READY'
assert-screen not_contains='7777'
