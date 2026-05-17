@name perf-idle
@record true
@render-trace true
@viewport cols=120 rows=40
@shell sh
@env-mode clean
@timeout 15000

new-session name=perf-idle
render-mark id='idle-start'
sleep ms=250
assert-render since='idle-start' max_frames=0 max_rows_emitted=0 max_cells_emitted=0 max_frame_bytes=0 full_frame=false expected_emitted_rows='' expected_emitted_row_segments='' expected_trace_ops=''
