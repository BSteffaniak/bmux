@driver attach-sim
@viewport cols=300 rows=24

seed-window-list names='one,two,three,four,five,six,seven,eight,nine,ten,eleven,twelve,thirteen,fourteen,fifteen' active='one'
render
assert-rendered contains='1:one'
assert-rendered contains='12:twelve'
assert-rendered contains='15:fifteen'
