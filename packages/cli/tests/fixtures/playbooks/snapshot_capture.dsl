@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo snap_content_marker\r'
wait-for pattern='snap_content_marker'
snapshot id=after_echo
