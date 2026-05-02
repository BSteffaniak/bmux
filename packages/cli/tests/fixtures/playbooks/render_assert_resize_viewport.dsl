@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='resize'
resize-viewport cols=100 rows=30
screen
assert-render since='resize' min_frames=1 max_frames=1 full_frame=true max_full_frame_frames=1 max_rows_emitted=30 max_cells_emitted=3000
