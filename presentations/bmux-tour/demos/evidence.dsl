@name Fixture, check, inspect, retain
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.tab_bar
new-session name='evidence'
send-attach key='ctrl+b'
# Create a disposable fixture inside this runner's owned runtime, never the repo.
# Match complete output lines so echoed commands cannot satisfy the checks.
send-attach key='escape'
paste-attach text='seq 1 42 > "$BMUX_RUNTIME_DIR/samples.txt"; wc -l < "$BMUX_RUNTIME_DIR/samples.txt"'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^42\s*$'
assert-layout pane_count=1
assert-screen matches='(?m)^42\s*$'
sleep ms=1800
send-attach key='%'
sleep ms=700
send-attach key='escape'
# A second pane independently reads the fixture: record count AND aggregate.
paste-attach text="awk '{sum += $1} END {print NR, sum}' \"$BMUX_RUNTIME_DIR/samples.txt\""
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^42 903\s*$'
assert-layout pane_count=2
assert-screen matches='(?m)^42 903\s*$'
# Keep a named snapshot only after both the structural and output checks pass.
snapshot id='verified-demo'
sleep ms=2200
