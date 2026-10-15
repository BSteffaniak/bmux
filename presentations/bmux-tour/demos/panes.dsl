@name A workspace in three panes
@viewport cols=80 rows=24
@shell sh
@timeout 30000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
new-session name='workspace-demo'
send-keys keys="printf '\\033[2J\\033[HAPP / ready\\n'\r"
wait-for pattern='APP / ready'
sleep ms=900
split-pane direction=vertical ratio=0.5
send-keys keys="printf '\\033[2J\\033[HTESTS / 12 passed\\n'\r"
wait-for pattern='TESTS / 12 passed'
sleep ms=900
split-pane direction=horizontal ratio=0.5
send-keys keys="printf '\\033[2J\\033[HLOGS / listening locally\\n'\r"
wait-for pattern='LOGS / listening locally'
assert-layout pane_count=3
sleep ms=1200
focus-pane target=0
assert-screen contains='APP / ready'
sleep ms=1200
focus-pane target=1
assert-screen contains='TESTS / 12 passed'
sleep ms=1200
# Completion retains the owned runtime; leaving the slide stops and cleans it.
