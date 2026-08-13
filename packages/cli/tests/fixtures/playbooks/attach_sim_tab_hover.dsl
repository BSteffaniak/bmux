@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three' active='one'
render
locate id='one' text='1:one'
locate id='two' text='2:two'

# Hovering a tab must not switch windows or start a reorder.
terminal-event kind=mouse phase=move col='${two.center_col}' row='${two.row}'
render
assert-rendered contains='two'
assert-state path='windows.names' equals='["one","two","three"]'
assert-state path='windows.active_name' equals='"one"'

# Moving off the status row leaves the tab strip intact.
terminal-event kind=mouse phase=move col='${two.center_col}' row='5'
render
assert-rendered contains='one'
assert-state path='windows.active_name' equals='"one"'
