@shell sh
new-session
send-keys keys='echo setup_done_marker\r'
wait-for pattern='setup_done_marker'
