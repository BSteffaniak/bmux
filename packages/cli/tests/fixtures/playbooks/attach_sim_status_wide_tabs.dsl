@driver attach-sim
@viewport cols=300 rows=24

seed-window-list names='one,two,three,four,five,six,seven,eight,nine,ten,eleven,twelve,thirteen,fourteen,fifteen' active='one'
render
assert-rendered contains='one'
assert-rendered contains='twelve'
assert-rendered contains='fifteen'
