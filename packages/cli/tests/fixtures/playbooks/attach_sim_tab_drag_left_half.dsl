@driver attach-sim
@viewport cols=100 rows=24

render
locate id='one' text='1:one'
locate id='three' text='3:three'

terminal-event kind=mouse phase=down button=left col='${three.center_col}' row='${three.row}'
terminal-event kind=mouse phase=drag button=left col='${one.start_col}' row='${one.row}'
terminal-event kind=mouse phase=up button=left col='${one.start_col}' row='${one.row}'

assert-effect operation='move-window'
assert-state path='windows.names' equals='["three","one","two"]'
