@viewport cols=80 rows=24
@shell sh
@var MARKER=default_value
new-session
send-keys keys='echo ${MARKER}\r'
wait-for pattern='cli_override'
