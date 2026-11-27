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
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS' timeout=60000
# A repository on a branch only this test uses, named without typing it
# (so the command line cannot satisfy the check). The shell reports its
# directory with OSC 7, as shell integration does.
send-keys keys='b=sb; b="${b}test"; cd "$(mktemp -d)" && git -c init.defaultBranch="$b" init -q . && printf "\\033]7;file://localhost%s\\007" "$PWD" && echo READY\r'
wait-for pattern='READY' timeout=60000
wait-rendered contains='sbtest' timeout=60000
# Only within the sidebar's columns.
assert-rendered matches='(?m)^.{0,28}sbtest'
