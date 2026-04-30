@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='split'
split-pane direction=vertical
assert-render since='split' min_frames=1 max_frames=1 full_frame=true max_full_frame_frames=1 max_rows_emitted=24 max_cells_emitted=1000
