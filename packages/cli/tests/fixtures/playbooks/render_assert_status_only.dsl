@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='status_check'
status
assert-render since='status_check' max_frames=0 max_rows_emitted=0 max_cells_emitted=0 full_frame=false expected_emitted_rows='' expected_emitted_row_segments='' expected_trace_ops=''
