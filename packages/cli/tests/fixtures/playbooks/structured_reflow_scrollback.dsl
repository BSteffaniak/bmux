@timeout 30000
@viewport cols=44 rows=8
@shell sh
@env-mode clean

new-session
send-keys keys='printf "\\e[2J\\e[H"; for i in 01 02 03 04 05 06 07 08 09 10 11 12 13 14; do printf "history-$i-abcdefghijklmnopqrstuvwxyz\\n"; done\r'
wait-for pattern='history-14'
resize-viewport cols=24 rows=8
screen
assert-screen scrollback=true matches='(?s)history-03-abcdefghijk\nlmnopqrstuvwxyz.*history-14-abcdefghijk\nlmnopqrstuvwxyz'
resize-viewport cols=44 rows=8
screen
assert-screen scrollback=true matches='(?s)history-03-abcdefghijklmnopqrstuvwxyz.*history-14-abcdefghijklmnopqrstuvwxyz'
