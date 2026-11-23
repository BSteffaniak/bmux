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
@plugin enable=bmux.agents
new-session
attach-command action='plugin:bmux.tabs:new-tab'
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS'
send-keys keys='set +H\r'
# A stand-in `claude` that sets Claude Code's busy title (a Braille spinner)
# and keeps printing: its screen rules say it is working.
send-keys keys='d=$(mktemp -d); T="\\033]0;\\342\\240\\202\\040Fixing\\007"; export T; printf "#!/bin/sh\nprintf %%b \$T\nwhile :; do echo thinking; sleep 0.3; done\n" > "$d/claude"; chmod +x "$d/claude"; PATH="$d:$PATH"; echo RE""ADY\r'
wait-for pattern='READY'
send-keys keys='claude\r'
wait-for pattern='thinking'
wait-rendered contains='working'
assert-rendered matches='(?m)^.{0,30}working'
