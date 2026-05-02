@timeout 30000
@viewport cols=64 rows=18
@shell sh
@env-mode clean

new-session
send-keys keys='printf "\\e[2J\\e[H"; printf "paneoneabcdefghijklmnopqrstuvwxyz0123456789\\n"\r' pane=1
wait-for pattern='6789' pane=1
split-pane direction=vertical
send-keys keys='printf "\\e[2J\\e[H"; printf "panetwo-ready\\n"\r' pane=2
wait-for pattern='panetwo-ready' pane=2
resize-viewport cols=42 rows=18
screen
assert-screen matches='paneoneabcdefghijkl\nmnopqrstuvwxyz01234\n56789' pane=1
assert-screen contains='panetwo-ready' pane=2
send-attach key='ctrl+a z'
sleep ms=250
screen
assert-screen contains='panetwo-ready' pane=1
send-attach key='ctrl+a z'
sleep ms=250
screen
assert-screen matches='paneoneabcdefghijkl\nmnopqrstuvwxyz01234\n56789' pane=1
assert-screen contains='panetwo-ready' pane=2
