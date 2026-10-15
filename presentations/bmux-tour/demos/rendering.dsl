@name Real output and scrollback
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
new-session name='rendering'
send-attach key='ctrl+b'
# Genuine utility output exercises terminal scrolling, not hand-painted ANSI rows.
send-attach key='escape'
paste-attach text='seq 1 100'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='100'
sleep ms=1800
send-attach key='['
sleep ms=1200
send-attach key='page_up'
sleep ms=1800
send-attach key='page_down'
sleep ms=1800
send-attach key='escape'
sleep ms=700
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1800
