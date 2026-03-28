@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo hello_world_test\r'
wait-for pattern='hello_world_test'
assert-screen contains='hello_world_test'
