@driver attach-sim
@viewport cols=100 rows=24

render
locate id='one' text='1:one'
locate id='two' text='2:two'

terminal-event kind=mouse phase=down button=left col='${one.center_col}' row='${one.row}'
terminal-event kind=mouse phase=drag button=left col='${two.end_col}' row='${two.row}'
terminal-event kind=mouse phase=up button=left col='${two.end_col}' row='${two.row}'

assert-effect operation='move-window'
assert-state path='windows.names' equals='["two","one","three"]'
