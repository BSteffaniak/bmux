@name Rendering is retained state
@viewport cols=80 rows=24
@shell sh
@timeout 30000
new-session name='rendering'
send-keys keys="printf '\\033[2J\\033[H'; i=1; while [ $i -le 18 ]; do printf 'event %02d | retained terminal output\\n' $i; i=$((i+1)); done\r"
wait-for pattern='event 18'
assert-screen contains='event 01'
sleep ms=1500
send-keys keys="printf '\\033[3;1H\\033[2Kevent 03 | updated in place\\n'\r"
wait-for pattern='updated in place'
sleep ms=1500
send-keys keys="printf '\\033[24;1H'; i=19; while [ $i -le 30 ]; do printf 'event %02d | scrolling continues\\n' $i; i=$((i+1)); done\r"
wait-for pattern='event 30'
assert-screen contains='scrolling continues'
sleep ms=1500
