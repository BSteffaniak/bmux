@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
split-pane direction=vertical
render-mark id='focus'
focus-pane target=1
assert-render since='focus' max_frames=0 max_rows_emitted=0 max_cells_emitted=0 full_frame=false expected_emitted_rows='' expected_emitted_row_segments=''
