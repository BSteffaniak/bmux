@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one' active='one'

send-attach key='ctrl+a ?'
assert-state path='help_overlay.open' equals='true'
assert-state path='help_overlay.scroll' equals='0'
assert-rendered contains='HELP'

send-attach key='page_down'
assert-state path='help_overlay.open' equals='true'
assert-state path='help_overlay.scroll' equals='18'

send-attach key='end'
assert-state path='help_overlay.open' equals='true'
assert-state path='help_overlay.scroll' equals='83'

send-attach key='home'
assert-state path='help_overlay.scroll' equals='0'

send-attach key='esc'
assert-state path='help_overlay.open' equals='false'
assert-state path='help_overlay.scroll' equals='0'
