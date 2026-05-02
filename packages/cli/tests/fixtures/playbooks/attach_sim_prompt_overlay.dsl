@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one' active='one'

send-attach key='ctrl+a ?'
assert-state path='help_overlay.open' equals='true'
assert-rendered contains='HELP'

send-attach key='esc'
assert-state path='help_overlay.open' equals='false'

send-attach key='ctrl+a q'
assert-state path='prompt.active' equals='true'
assert-rendered contains='PROMPT'

send-attach key='esc'
assert-state path='prompt.active' equals='false'
