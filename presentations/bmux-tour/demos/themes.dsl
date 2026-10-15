@name Theme selection is real runtime state
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
@plugin enable=bmux.decoration
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.theme
new-session name='themes'
send-attach key='ctrl+b'
send-attach key='escape'
paste-attach text='ls -d packages plugins presentations'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
sleep ms=1800
send-attach key='ctrl+alt+t'
wait-rendered contains='Select Theme'
sleep ms=1800
send-attach key='down'
sleep ms=1800
wait-rendered contains='cyberpunk'
send-attach key='enter'
sleep ms=1800
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1800
