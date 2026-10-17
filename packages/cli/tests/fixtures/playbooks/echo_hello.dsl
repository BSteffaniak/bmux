@viewport cols=80 rows=24
@shell sh
new-session
# Only executed output contains the contiguous sentinel; terminal echo cannot
# satisfy the wait before the command runs.
send-keys keys='printf "%s%s\\n" hello_world_ test\r'
wait-for pattern='(?m)^hello_world_test *$'
assert-screen contains='hello_world_test'
