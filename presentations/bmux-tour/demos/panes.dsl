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
# Discover the repository before splitting: implementation, plugins, and tour source.
send-attach key='escape'
paste-attach text='ls -d packages plugins presentations'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
assert-layout pane_count=1
assert-screen contains='packages'
sleep ms=1800
# Default runtime shortcuts, dispatched by the actual owned attach.
send-attach key='%'
sleep ms=700
send-attach key='escape'
paste-attach text='uname -sr'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^Linux '
assert-layout pane_count=2
assert-screen matches='(?m)^Linux '
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
assert-screen matches='(?m)^.* UTC [0-9]{4}'
snapshot id='three-pane-workspace'
sleep ms=1800
send-attach key='alt+h'
sleep ms=1600
send-attach key='alt+t'
sleep ms=1600
# Completion retains the owned runtime; leaving the slide stops and cleans it.
