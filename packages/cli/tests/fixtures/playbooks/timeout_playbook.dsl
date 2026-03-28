@viewport cols=80 rows=24
@shell sh
@timeout 1500
new-session
sleep ms=3000
assert-screen contains='anything'
