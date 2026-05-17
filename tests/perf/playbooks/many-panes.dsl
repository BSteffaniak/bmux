@name perf-many-panes
@record true
@driver real-attach
@render-trace true
@viewport cols=160 rows=48
@shell sh
@env-mode clean
@timeout 25000

new-session name=perf-many-panes
split-pane direction=vertical
focus-pane target=1
split-pane direction=horizontal
focus-pane target=2
split-pane direction=horizontal
render-mark id='many-start'
send-keys keys='for i in 1 2 3 4 5; do echo PANE1_$i; done\r' pane=1
send-keys keys='for i in 1 2 3 4 5; do echo PANE2_$i; done\r' pane=2
send-keys keys='for i in 1 2 3 4 5; do echo PANE3_$i; done\r' pane=3
send-keys keys='for i in 1 2 3 4 5; do echo PANE4_$i; done\r' pane=4
wait-for pattern='PANE1_5' pane=1 timeout=5000
wait-for pattern='PANE2_5' pane=2 timeout=5000
wait-for pattern='PANE3_5' pane=3 timeout=5000
wait-for pattern='PANE4_5' pane=4 timeout=5000
assert-render since='many-start' full_frame=false max_full_frame_frames=0 max_full_surface_fallbacks=0 max_frame_bytes=200000
