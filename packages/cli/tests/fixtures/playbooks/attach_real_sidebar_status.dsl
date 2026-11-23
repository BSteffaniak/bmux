@driver real-attach
@timeout 60000
@viewport cols=110 rows=30
@shell sh
@plugin enable=bmux.tabs
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.permissions
@plugin enable=bmux.sidebar
new-session
send-attach key='ctrl+alt+s'
wait-rendered contains='TABS'
# A repository on a branch only this test uses; the shell reports its
# directory with OSC 7, as shell integration does.
send-keys keys='cd "$(mktemp -d)" && git -c init.defaultBranch=sbtest init -q . && printf "\033]7;file://localhost%s\033\\" "$PWD" && echo READY\r'
wait-for pattern='READY'
wait-rendered contains='sbtest'
