@driver attach-sim
@viewport cols=100 rows=24

render
locate id='three' text='three'

# Right-click opens the context menu for the clicked tab without switching.
terminal-event kind=mouse phase=down button=right col='${three.center_col}' row='${three.row}'
render
assert-state path='tab_menu.open' equals='true'
assert-state path='windows.active_name' equals='"one"'
assert-state path='tab_menu.focused' equals='"rename"'

# The last tab cannot move right, and the entry is disabled rather than hidden.
assert-state path='tab_menu.items' equals='["rename","close","move-left","move-right:disabled","move-to-first","move-to-last:disabled","new-window"]'

# Escape dismisses without acting.
send-attach key='Esc'
assert-state path='tab_menu.open' equals='false'
assert-state path='windows.names' equals='["one","two","three"]'

# Reopen and move the tab to the front.
terminal-event kind=mouse phase=down button=right col='${three.center_col}' row='${three.row}'
send-attach key='Down'
send-attach key='Down'
send-attach key='Down'
assert-state path='tab_menu.focused' equals='"move-to-first"'
send-attach key='Enter'

assert-state path='tab_menu.open' equals='false'
assert-effect operation='move-window'
assert-state path='windows.names' equals='["three","one","two"]'
