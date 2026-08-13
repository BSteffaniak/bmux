@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one,two,three' active='one'
set-config path='status_bar.tab_template' value='[{name}]'
render
locate id='two' text='two'

# Double-click opens the inline editor with the raw name (template chrome
# removed) and the whole name selected.
terminal-event kind=mouse phase=down button=left col='${two.center_col}' row='${two.row}'
terminal-event kind=mouse phase=up button=left col='${two.center_col}' row='${two.row}'
terminal-event kind=mouse phase=down button=left col='${two.center_col}' row='${two.row}'
render
assert-state path='tab_rename.active' equals='true'
assert-state path='tab_rename.text' equals='"two"'

# Typing replaces the selected name, then Enter commits.
send-attach key='dev'
assert-state path='tab_rename.text' equals='"dev"'
send-attach key='Enter'
render

assert-state path='tab_rename.active' equals='false'
assert-effect operation='rename-window'
assert-state path='windows.names' equals='["one","dev","three"]'
# Template chrome returns once editing ends.
assert-rendered contains='[dev]'
