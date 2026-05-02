@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='after_boot'
send-keys keys='echo bmux_render_one_line\r'
wait-for pattern='bmux_render_one_line'
assert-render since='after_boot' min_frames=1 max_frames=2 full_frame=false max_full_frame_frames=0 max_rows_emitted=4 max_cells_emitted=320
