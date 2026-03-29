@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo real_output\r'
wait-for pattern='real_output'
assert-screen contains='nonexistent_xyz_continue' !continue
assert-screen contains='real_output'
