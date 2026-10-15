@name A demonstration with checks
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.tab_bar
new-session name='evidence'
send-attach key='ctrl+b'
# Verify real computation. The expected answer is absent from the typed command.
send-attach key='escape'
paste-attach text='expr 19 + 23'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='42'
assert-layout pane_count=1
assert-screen contains='42'
sleep ms=1800
send-attach key='%'
sleep ms=700
send-attach key='escape'
paste-attach text='expr 6 \* 7'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='42'
assert-layout pane_count=2
assert-screen contains='42'
snapshot id='verified-demo'
sleep ms=2200
