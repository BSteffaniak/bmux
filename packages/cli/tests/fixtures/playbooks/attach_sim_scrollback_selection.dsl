@driver attach-sim
@viewport cols=100 rows=24

seed-pane-text lines='one|  four|     five|  six' cursor_row=4 cursor_col=3

send-attach key='ctrl+a ['
assert-state path='scrollback.active' equals='true'
assert-state path='scrollback.cursor' equals='[3,2]'

send-attach key='v'
send-attach key='k'
assert-state path='selection.active' equals='true'
assert-state path='scrollback.cursor' equals='[2,2]'
# Anchor at row 3 col 2, head at row 2 col 2: covers `     five` from col 2
# through `  six` col 2.
assert-state path='selection.text' equals='"   five\n  s"'
