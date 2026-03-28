@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo test_success_marker\r'
wait-for pattern='test_success_marker'
assert-screen contains='test_success_marker'
assert-screen not_contains='test_failure_marker'
