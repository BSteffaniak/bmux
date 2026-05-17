@name perf-focus-churn
@record true
@render-trace true
@viewport cols=120 rows=40
@shell sh
@env-mode clean
@timeout 20000

new-session name=perf-focus-churn
split-pane direction=vertical
focus-pane target=1
split-pane direction=horizontal
send-keys keys='printf "pane1-ready\n"\r' pane=1
send-keys keys='printf "pane2-ready\n"\r' pane=2
send-keys keys='printf "pane3-ready\n"\r' pane=3
wait-for pattern='pane1-ready' pane=1 timeout=5000
wait-for pattern='pane2-ready' pane=2 timeout=5000
wait-for pattern='pane3-ready' pane=3 timeout=5000
render-mark id='focus-start'
focus-pane target=1
focus-pane target=2
focus-pane target=1
focus-pane target=2
focus-pane target=1
focus-pane target=2
assert-render since='focus-start' max_full_surface_fallbacks=0 max_frame_bytes=120000
