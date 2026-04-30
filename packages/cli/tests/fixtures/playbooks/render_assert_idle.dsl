@render-trace true
new-session
render-mark id='baseline'
sleep ms=10
assert-render since='baseline' max_frames=0 max_rows_emitted=0 max_cells_emitted=0 full_frame=false expected_emitted_rows=''
