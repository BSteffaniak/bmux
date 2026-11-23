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
# A repository whose configuration runs a command during `git status`.
send-keys keys='d=$(mktemp -d)/plant""ed; mkdir -p "$d" && git -C "$d" init -q && git -C "$d" config core.fsmonitor "touch $d/../RAN; false" && echo RE""ADY\r'
wait-for pattern='READY'
# Output names it with OSC 7 without the shell entering it: the sidebar may
# show it while the command runs, but must never run git there.
send-keys keys='printf "\\033]7;file://localhost%s\\007" "$d"; sleep 4; echo DO""NE\r'
wait-rendered contains='planted'
assert-rendered matches='(?m)^.{0,30}planted'
wait-for pattern='DONE'
send-keys keys='test -e "$d/../RAN" && echo LE""AKED || echo SA""FE\r'
wait-for pattern='SAFE'
