@name alternate-screen-resize-shell-liveness
@timeout 30000
@viewport cols=40 rows=10
@shell sh
@env-mode clean

new-session
send-keys keys='printf "SHELL_BEFORE\\n"; printf "\\e[?1049h\\e[2J\\e[HALT_ACTIVE"; sleep 1; printf "\\e[?1049l"; printf "SHELL_RETURNED\\n"\r'
wait-for pattern='ALT_ACTIVE'
resize-viewport cols=80 rows=24
wait-for pattern='SHELL_RETURNED'
send-keys keys='printf "PANE_STILL_ALIVE\\n"\r'
wait-for pattern='PANE_STILL_ALIVE'
screen
assert-screen contains='SHELL_RETURNED'
assert-screen contains='PANE_STILL_ALIVE'
