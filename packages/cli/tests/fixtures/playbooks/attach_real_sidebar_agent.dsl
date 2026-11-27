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
# A stand-in `claude` that animates Claude Code's busy title (a Braille
# spinner) and keeps printing: its screen rules say it is working. The
# spinner must keep changing, as Claude's does: a title the pane already had
# when the agent was identified is not taken as the agent's own. The script
# spells `thinking` with an octal escape (`\153` is `k`) so the typed line
# that creates it cannot satisfy the wait for the running stand-in's output.
send-keys keys='d=$(mktemp -d); A="\\033]0;\\342\\240\\202\\040Fixing\\007"; B="\\033]0;\\342\\240\\220\\040Fixing\\007"; export A B; printf "#!/bin/sh\nwhile :; do printf %%b \$A; echo thin\\153ing; sleep 0.3; printf %%b \$B; echo thin\\153ing; sleep 0.3; done\n" > "$d/claude"; chmod +x "$d/claude"; PATH="$d:$PATH"; echo RE""ADY\r'
wait-for pattern='READY' timeout=60000
send-keys keys='claude\r'
wait-for pattern='thinking' timeout=60000
wait-rendered contains='working' timeout=60000
assert-rendered matches='(?m)^.{0,30}working'
