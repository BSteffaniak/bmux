@driver attach-sim
@viewport cols=100 rows=24

set-config path='status_bar.tab_order' value='mru'
render
locate id='one' text='1:one'
locate id='three' text='3:three'

terminal-event kind=mouse phase=down button=left col='${one.center_col}' row='${one.row}'
terminal-event kind=mouse phase=move button=left col='${three.end_col}' row='${three.row}'
terminal-event kind=mouse phase=up button=left col='${three.end_col}' row='${three.row}'

assert-no-effect operation='move-window'
assert-state path='windows.names' equals='["one","two","three"]'
