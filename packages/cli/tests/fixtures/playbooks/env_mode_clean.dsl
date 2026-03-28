@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
send-keys keys='echo $TERM\r'
wait-for pattern='xterm-256color'
assert-screen contains='xterm-256color'
