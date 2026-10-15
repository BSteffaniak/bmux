@name Find work by intent
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 60000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
@plugin enable=bmux.finder
new-session name='api'
send-attach key='ctrl+b'
send-attach key='escape'
paste-attach text='uname -sr'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^Linux '
assert-screen matches='(?m)^Linux '
sleep ms=1500
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1500
send-attach key='shift+w'
sleep ms=1000
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='pwd'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
sleep ms=1500
# The same default shortcut a user presses; prompt input stays in its modal scope.
send-attach key='f'
wait-rendered contains='Find Tab'
sleep ms=1500
send-attach key='a'
sleep ms=250
send-attach key='p'
sleep ms=250
send-attach key='i'
sleep ms=1800
wait-rendered contains='api'
send-attach key='enter'
sleep ms=700
# Selection must restore the actual API terminal, not merely match a picker label.
assert-screen matches='(?m)^Linux '
assert-layout pane_count=1
snapshot id='api-found'
sleep ms=2000
