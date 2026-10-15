@name Find work by intent
@viewport cols=80 rows=24
@shell sh
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
@plugin enable=bmux.finder
new-session name='api'
send-keys keys="printf '\\033[2J\\033[HAPI / ready\\n'\r"
wait-for pattern='API / ready'
# Invoke actual attach actions without depending on the presenter's keybindings.
attach-command action='plugin:bmux.tabs:new-tab --name build'
sleep ms=700
send-keys keys="printf '\\033[2J\\033[HBUILD / ready\\n'\r"
wait-for pattern='BUILD / ready'
attach-command action='plugin:bmux.workspaces:new-workspace --name operations'
sleep ms=700
attach-command action='plugin:bmux.tabs:new-tab --name logs'
sleep ms=700
send-keys keys="printf '\\033[2J\\033[HLOGS / ready\\n'\r"
wait-for pattern='LOGS / ready'
sleep ms=1000
attach-command action='plugin:bmux.finder:show'
sleep ms=1000
wait-rendered contains='Find Tab'
send-attach key='a'
send-attach key='p'
send-attach key='i'
sleep ms=1500
wait-rendered contains='api'
send-attach key='enter'
sleep ms=700
assert-screen contains='API / ready'
sleep ms=1500
