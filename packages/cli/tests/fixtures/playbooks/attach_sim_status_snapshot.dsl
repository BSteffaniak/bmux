@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three' active='one'
render
snapshot id='initial-status'
assert-rendered contains='one'
