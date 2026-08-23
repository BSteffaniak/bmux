@driver attach-sim
@viewport cols=100 rows=24

render
snapshot id='initial-status'
assert-rendered contains='one'
