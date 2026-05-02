@render-trace true
@viewport cols=40 rows=8
@shell sh
@env-mode clean
new-session
render-mark id='after_boot'
send-keys keys='echo bmux_render_one_line\r'
wait-for pattern='bmux_render_one_line'
assert-render since='after_boot' min_frames=1 max_frames=1 full_frame=false max_full_frame_frames=0 max_rows_emitted=3 max_cells_emitted=190
