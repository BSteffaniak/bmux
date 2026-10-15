@name Work keeps running while views change
@viewport cols=80 rows=24
@shell sh
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
new-session name='api'
send-keys keys="printf '\\033[2J\\033[HAPI / original shell\\n'; demo_identity=still-here\r"
wait-for pattern='API / original shell'
sleep ms=1200
# Normal-mode c invokes the real tabs plugin command.
attach-command action='plugin:bmux.tabs:new-tab --name tests'
sleep ms=700
send-keys keys="printf '\\033[2J\\033[HTESTS / another tab\\n'\r"
wait-for pattern='TESTS / another tab'
sleep ms=1200
attach-command action='plugin:bmux.tabs:prev-tab'
sleep ms=700
send-keys keys="printf 'API state: %s\\n' \"$demo_identity\"\r"
wait-for pattern='API state: still-here'
assert-screen contains='API / original shell'
sleep ms=1200
attach-command action='plugin:bmux.tabs:next-tab'
sleep ms=700
sleep ms=1200
attach-command action='plugin:bmux.workspaces:new-workspace --name operations'
sleep ms=700
attach-command action='plugin:bmux.tabs:new-tab --name deploy'
sleep ms=700
send-keys keys="printf '\\033[2J\\033[HDEPLOY / a different workspace\\n'\r"
wait-for pattern='DEPLOY / a different workspace'
sleep ms=1200
attach-command action='plugin:bmux.workspaces:last-workspace'
sleep ms=700
assert-screen contains='TESTS / another tab'
sleep ms=1200
