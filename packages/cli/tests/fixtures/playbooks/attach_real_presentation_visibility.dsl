@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@plugin enable=bmux.tabs
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.permissions
@plugin enable=bmux.sidebar
new-session
# The sidebar starts hidden by default, so output begins at
# column 0. Markers are printed in two halves so the echoed command line
# cannot match.
send-keys keys='clear; printf %s%s HID DEN_OK\r'
wait-rendered contains='HIDDEN_OK'
assert-rendered matches='(?m)^ *HIDDEN_OK'
send-attach key='ctrl+alt+s'
wait-rendered contains=' TABS '
send-attach key='ctrl+alt+s'
send-keys keys='clear; printf %s%s HIDDEN AGAIN_OK\r'
wait-rendered contains='HIDDENAGAIN_OK'
assert-rendered matches='(?m)^ *HIDDENAGAIN_OK'
