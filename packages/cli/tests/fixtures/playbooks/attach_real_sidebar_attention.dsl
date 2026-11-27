@driver real-attach
@timeout 300000
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
wait-rendered contains='TABS' timeout=60000
# The second tab rings the bell after we have switched away from it.
send-keys keys='(sleep 2; printf "\a") & echo READY\r'
wait-for pattern='READY' timeout=60000
attach-command action='plugin:bmux.tabs:prev-tab'
wait-rendered contains='needs input' timeout=60000
assert-rendered matches='(?m)^.{0,30}needs input'
