@driver attach-sim
@viewport cols=100 rows=24

seed-window-list names='one' active='one'
seed-pane-layout split='floating'

terminal-event kind=mouse phase=down button=left col=2 row=2
terminal-event kind=mouse phase=drag button=left col=6 row=4
terminal-event kind=mouse phase=up button=left col=6 row=4

assert-effect operation='focus-pane'
assert-effect operation='move-floating-pane'
