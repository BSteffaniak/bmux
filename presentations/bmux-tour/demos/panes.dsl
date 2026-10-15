@name A workspace in three panes
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
new-session name='workspace-demo'
send-attach key='ctrl+b'
# Ordinary commands are visible; no fabricated application or test output.
send-attach key='escape'
paste-attach text='ls -d packages plugins presentations'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
sleep ms=1800
# Default runtime shortcuts, dispatched by the actual owned attach.
send-attach key='%'
sleep ms=700
send-attach key='escape'
paste-attach text='uname -sr'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='Linux'
sleep ms=1800
send-attach key='"'
sleep ms=700
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
assert-layout pane_count=3
sleep ms=1800
send-attach key='alt+h'
sleep ms=1600
send-attach key='alt+t'
sleep ms=1600
# Completion retains the owned runtime; leaving the slide stops and cleans it.
