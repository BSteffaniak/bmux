@driver attach-sim
@viewport cols=300 rows=24

render
assert-rendered contains='one'
assert-rendered contains='twelve'
assert-rendered contains='fifteen'
