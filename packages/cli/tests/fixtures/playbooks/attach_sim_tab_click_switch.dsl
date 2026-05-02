@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three' active='one'
render
locate id='two' text='2:two'

terminal-event kind=mouse phase=down button=left col='${two.center_col}' row='${two.row}'
terminal-event kind=mouse phase=up button=left col='${two.center_col}' row='${two.row}'

assert-effect operation='switch-window'
assert-no-effect operation='move-window'
assert-state path='windows.names' equals='["one","two","three"]'
assert-state path='windows.active_name' equals='"two"'
