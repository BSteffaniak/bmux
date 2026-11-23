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
# A job reports, with the pane's marker, that it is blocked: the sidebar
# says so with its message (assembled here, so it is never typed).
send-keys keys='a=app; M=$(printf ${a}rove | base64); R="\\033]633;bmux;$BMUX_SHELL_MARKER;agent;blocked;;$M\\007"; export R; echo RE""ADY\r'
wait-for pattern='READY'
send-keys keys='sh -c "printf %b \$R; sleep 8"\r'
wait-rendered contains='needs input'
assert-rendered matches='(?m)^.{0,30}needs input'
wait-rendered contains='approve'
assert-rendered matches='(?m)^.{0,30}approve'
