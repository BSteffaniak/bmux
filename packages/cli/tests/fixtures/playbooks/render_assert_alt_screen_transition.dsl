@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='alt_transition'
send-keys keys='printf "main"; printf "\\e[?1049h"; printf "ALT_ONLY"; printf "\\e[?1049l"; printf "ALT_EXIT_READY"; cat\r'
wait-for pattern='ALT_EXIT_READY'
assert-screen contains='ALT_EXIT_READY'
assert-render since='alt_transition' min_frames=1 max_frames=3 max_full_frame_frames=1 max_rows_emitted=4 max_cells_emitted=320 expected_emitted_rows='1:0,1:1,1:2' expected_emitted_row_segments='1:0:0:78,1:1:0:78,1:2:0:78' expected_trace_ops='cursor:1:true,pane-row-segment:1:0:0:78,pane-row-segment:1:1:0:78,pane-row-segment:1:2:0:78,pane-row-cache-skip:1:3,pane-row-cache-skip:1:4,pane-row-cache-skip:1:5,pane-row-cache-skip:1:6,pane-row-cache-skip:1:7,pane-row-cache-skip:1:8,pane-row-cache-skip:1:9,pane-row-cache-skip:1:10,pane-row-cache-skip:1:11,pane-row-cache-skip:1:12,pane-row-cache-skip:1:13,pane-row-cache-skip:1:14,pane-row-cache-skip:1:15,pane-row-cache-skip:1:16,pane-row-cache-skip:1:17,pane-row-cache-skip:1:18,pane-row-cache-skip:1:19,pane-row-cache-skip:1:20,pane-row-cache-skip:1:21'
