@name Theme selection is real runtime state
@viewport cols=80 rows=24
@shell sh
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
@plugin enable=bmux.decoration
@plugin enable=bmux.pane_runtime
@plugin enable=bmux.theme
new-session name='themes'
send-keys keys="printf '\\033[2J\\033[HOne running shell / different visual compositions\\n'\r"
wait-for pattern='different visual compositions'
sleep ms=1000
attach-command action='plugin:bmux.theme:pick-theme'
wait-rendered contains='Select Theme'
sleep ms=1200
# This is a single-select preview list, not the finder's search prompt.
send-attach key='down'
sleep ms=1500
wait-rendered contains='cyberpunk'
send-attach key='enter'
sleep ms=2000
send-keys keys="printf 'Theme changed / shell still running\\n'\r"
wait-for pattern='Theme changed / shell still running'
sleep ms=1500
