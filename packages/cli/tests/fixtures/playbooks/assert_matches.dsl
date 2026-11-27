@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo "count: 42 items"\r'
wait-for pattern='count:' timeout=60000
assert-screen matches='count: \d+ items'
