@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo real_output\r'
wait-for pattern='real_output' timeout=60000
assert-screen matches='IMPOSSIBLE_REGEX_xyz\d{999}'
