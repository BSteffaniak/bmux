@render-trace true
@viewport cols=40 rows=8
@shell sh
@env-mode clean
new-session
render-mark id='after_boot'
send-keys keys='echo bmux_render_one_line\r'
wait-for pattern='bmux_render_one_line'
assert-render since='after_boot' min_frames=1 max_frames=1 full_frame=false max_full_frame_frames=0 max_rows_emitted=3 max_cells_emitted=114 expected_emitted_rows='1:0,1:1,1:2' expected_emitted_row_segments='1:0:0:33,1:1:0:33,1:2:0:33' expected_trace_ops='cursor:1:true,pane-row-segment:1:0:0:33,pane-row-segment:1:1:0:33,pane-row-segment:1:2:0:33,pane-row-cache-skip:1:3,pane-row-cache-skip:1:4,pane-row-cache-skip:1:5'
