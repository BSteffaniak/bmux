@name Work keeps running while views change
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
new-session name='api'
send-attach key='ctrl+b'
# Remember actual shell identity, not a printed pretend status.
send-attach key='escape'
paste-attach text='tour_origin=$$; pwd'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
sleep ms=1800
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1800
send-attach key='ctrl+h'
sleep ms=700
send-attach key='escape'
paste-attach text='test "$tour_origin" = "$$" && expr 20 + 22'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='42'
sleep ms=1800
send-attach key='ctrl+s'
sleep ms=1600
# Create a real workspace using its default shortcut and generated name.
send-attach key='shift+w'
sleep ms=1000
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='uname -sr'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='Linux'
sleep ms=1800
send-attach key='alt+l'
sleep ms=700
assert-screen contains='UTC'
sleep ms=1800
