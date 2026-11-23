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
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS'
# A stand-in agent named `claude` that keeps producing output. Shell
# integration reports it as the running command.
send-keys keys='set +H\r'
send-keys keys='d=$(mktemp -d); printf "#!/bin/sh\nwhile :; do echo thinking; sleep 0.3; done\n" > "$d/claude"; chmod +x "$d/claude"; PATH="$d:$PATH"; echo READY\r'
wait-for pattern='READY'
send-keys keys='claude\r'
wait-for pattern='thinking'
wait-rendered contains='working'
assert-rendered matches='(?m)^.{0,30}working'
