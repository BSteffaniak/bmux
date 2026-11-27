@name alternate-screen-resize-shell-liveness
@timeout 300000
@viewport cols=40 rows=10
@shell sh
@env-mode clean

new-session
send-keys keys='printf "SHELL_BEFORE\\n"; printf "\\e[?1049h\\e[2J\\e[HALT_ACTIVE"; sleep 1; printf "\\e[?1049l"; printf "SHELL_RETURNED\\n"\r'
wait-for pattern='ALT_ACTIVE' timeout=60000
resize-viewport cols=80 rows=24
wait-for pattern='SHELL_RETURNED' timeout=60000
send-keys keys='printf "PANE_STILL_ALIVE\\n"\r'
wait-for pattern='PANE_STILL_ALIVE' timeout=60000
screen
assert-screen contains='SHELL_RETURNED'
assert-screen contains='PANE_STILL_ALIVE'
