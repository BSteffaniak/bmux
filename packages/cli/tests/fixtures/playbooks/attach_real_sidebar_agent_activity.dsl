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
@plugin enable=bmux.agents
new-session
attach-command action='plugin:bmux.tabs:new-tab'
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS' timeout=60000
send-keys keys='set +H\r'
# An agent known by name only (no screen rules) works while it prints.
send-keys keys='d=$(mktemp -d); printf "#!/bin/sh\nwhile :; do echo thinking; sleep 0.3; done\n" > "$d/aider"; chmod +x "$d/aider"; PATH="$d:$PATH"; echo RE""ADY\r'
wait-for pattern='READY' timeout=60000
send-keys keys='aider\r'
wait-for pattern='thinking' timeout=60000
wait-rendered contains='working' timeout=60000
assert-rendered matches='(?m)^.{0,30}working'
