@viewport cols=80 rows=24
@shell sh
@timeout 10000
new-session
wait-for pattern='THIS_WILL_NEVER_MATCH_xyz' timeout=1000
