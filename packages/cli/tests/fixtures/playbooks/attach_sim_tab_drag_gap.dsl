@driver attach-sim
@viewport cols=100 rows=24

render
locate id='one' text='1:one'

terminal-event kind=mouse phase=down button=left col='${one.center_col}' row='${one.row}'
terminal-event kind=mouse phase=move button=left col=99 row='${one.row}'
terminal-event kind=mouse phase=up button=left col=99 row='${one.row}'

assert-effect operation='move-window'
assert-state path='windows.names' equals='["two","three","one"]'
