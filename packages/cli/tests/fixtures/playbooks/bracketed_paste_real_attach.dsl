@driver real-attach
@viewport cols=100 rows=30
@shell sh
new-session
send-keys keys='dd bs=1 count=9 of=/tmp/bmux-paste-raw.bin 2>/dev/null\r'
paste-attach text='raw paste'
wait-for pattern='sh-'
send-keys keys="printf 'RAW_HEX='; od -An -tx1 /tmp/bmux-paste-raw.bin | tr -d ' \\n'; echo\r"
wait-for pattern='RAW_HEX=726177207061737465'
assert-screen contains='RAW_HEX=726177207061737465'
send-keys keys="printf '\\033[?2004h'; dd bs=1 count=25 of=/tmp/bmux-paste-wrapped.bin 2>/dev/null\r"
sleep ms=100
paste-attach text='wrapped paste'
wait-for pattern='sh-'
send-keys keys="printf 'WRAPPED_HEX='; od -An -tx1 /tmp/bmux-paste-wrapped.bin | tr -d ' \\n'; echo\r"
wait-for pattern='WRAPPED_HEX=1b5b3230307e777261707065642070617374651b5b3230317e'
assert-screen contains='WRAPPED_HEX=1b5b3230307e777261707065642070617374651b5b3230317e'
