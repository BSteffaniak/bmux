@render-trace true
@viewport cols=80 rows=24
@shell sh
@env-mode clean
new-session
render-mark id='alt_transition'
send-keys keys='printf "main"; printf "\\e[?1049h"; printf "ALT_ONLY"; printf "\\e[?1049l"; printf "ALT_EXIT_READY"; cat\r'
wait-for pattern='ALT_EXIT_READY'
assert-screen contains='ALT_EXIT_READY'
assert-render since='alt_transition' min_frames=1 max_frames=3 max_full_frame_frames=1 max_rows_emitted=5 max_cells_emitted=400
