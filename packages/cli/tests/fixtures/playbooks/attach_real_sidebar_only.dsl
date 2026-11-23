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
# The sidebar is enabled by default but starts hidden.
send-attach key='ctrl+alt+s'
wait-rendered contains=' TABS '
# The sidebar's heading and tab count head its first row.
assert-rendered matches='(?m)^ TABS +[0-9]'
assert-screen contains='SIDEBAR_ONLY_OK'
send-attach key='ctrl+a c'
send-attach key='ctrl+s'
resize-viewport cols=80 rows=24
resize-viewport cols=120 rows=36
