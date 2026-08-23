@driver attach-sim
@viewport cols=100 rows=24

seed-pane-layout split='vertical'

terminal-event kind=mouse phase=down button=left col=9 row=3
terminal-event kind=mouse phase=drag button=left col=12 row=3
terminal-event kind=mouse phase=up button=left col=12 row=3

assert-effect operation='resize-pane'
