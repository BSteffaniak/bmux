@driver real-attach
@viewport cols=100 rows=30
@shell sh
@plugin enable=bmux.tabs
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.permissions
@plugin enable=bmux.sidebar
@plugin disable=bmux.tab_bar
new-session
send-keys keys='printf SIDEBAR_ONLY_OK\r'
wait-for pattern='SIDEBAR_ONLY_OK'
wait-rendered contains=' Tabs '
# With the tab bar disabled, the sidebar's bottom border is the last row.
assert-rendered matches='└─+┘ *$'
assert-screen contains='SIDEBAR_ONLY_OK'
send-attach key='ctrl+a c'
send-attach key='ctrl+s'
resize-viewport cols=80 rows=24
resize-viewport cols=120 rows=36
