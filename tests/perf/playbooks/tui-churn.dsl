@name perf-tui-churn
@record true
@render-trace true
@viewport cols=120 rows=40
@shell sh
@env-mode clean
@timeout 20000

new-session name=perf-tui-churn
render-mark id='after-startup'
send-keys keys='i=0; while [ $i -lt 40 ]; do printf "CHURN_%02d abcdefghijklmnopqrstuvwxyz ABCDEFGHIJKLMNOPQRSTUVWXYZ\n" "$i"; i=$((i+1)); done\r'
wait-for pattern='CHURN_39' timeout=8000
assert-render since='after-startup' full_frame=false max_full_frame_frames=0 max_full_surface_fallbacks=0 max_frame_bytes=80000
