@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three,four,five,six,seven,eight,nine,ten,eleven,twelve' active='twelve'
render
assert-rendered contains='twelve'

resize-viewport cols=44 rows=16
render
assert-rendered contains='twelve'
assert-state path='windows.active_name' equals='"twelve"'

resize-viewport cols=120 rows=30
render
assert-rendered contains='twelve'
assert-state path='windows.active_name' equals='"twelve"'
