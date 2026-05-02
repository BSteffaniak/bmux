@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three' active='one'
render
locate id='one' text='1:one'
locate id='three' text='3:three'

terminal-event kind=mouse phase=down button=left col='${one.center_col}' row='${one.row}'
terminal-event kind=mouse phase=up button=left col='${three.end_col}' row='${three.row}'

assert-effect operation='move-window'
assert-state path='windows.names' equals='["two","three","one"]'
