@driver real-attach
@viewport cols=100 rows=30
@shell sh
@plugin enable=bmux.windows
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.permissions
@plugin enable=bmux.tab_strip
new-session
send-keys keys='printf TAB_ONLY_OK\r'
wait-for pattern='TAB_ONLY_OK'
assert-screen contains='TAB_ONLY_OK'
send-attach key='ctrl+a c'
send-attach key='ctrl+s'
resize-viewport cols=80 rows=24
resize-viewport cols=120 rows=36
