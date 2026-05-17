@name perf-layout-churn
@record true
@render-trace true
@viewport cols=120 rows=40
@shell sh
@env-mode clean
@timeout 20000

new-session name=perf-layout-churn
send-keys keys='printf "layout-start\n"\r'
wait-for pattern='layout-start' timeout=5000
render-mark id='layout-start'
split-pane direction=vertical
close-pane
split-pane direction=horizontal
close-pane
assert-render since='layout-start' max_full_surface_fallbacks=0 max_frame_bytes=200000
