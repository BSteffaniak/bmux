@driver real-attach
@timeout 60000
@viewport cols=110 rows=30
@shell bash
@plugin enable=bmux.tabs
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.permissions
@plugin enable=bmux.sidebar
new-session
attach-command action='plugin:bmux.tabs:new-tab'
attach-command action='plugin:bmux.tabs:new-tab'
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS'
# The second tab rings the bell after we have switched away from it.
send-keys keys='(sleep 2; printf "\a") & echo READY\r'
wait-for pattern='READY'
attach-command action='plugin:bmux.tabs:prev-tab'
wait-rendered contains='needs input'
assert-rendered matches='(?m)^.{0,30}needs input'
