@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo hello\r'
wait-for pattern='hello'
assert-screen matches='[invalid regex'
