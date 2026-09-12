@driver real-attach
@viewport cols=100 rows=30
@shell sh
new-session
send-keys keys="saved=$(stty -g); stty -echo -icanon min 1 time 0; printf 'RAW_%s\\n' READY; hex=$(dd bs=1 count=9 2>/dev/null | od -An -tx1 | tr -d ' \\n'); stty \"$saved\"; printf 'RAW_HEX=%s\\n' \"$hex\"\r"
wait-for pattern='RAW_READY'
paste-attach text='raw paste'
wait-for pattern='RAW_HEX=726177207061737465'
assert-screen contains='RAW_HEX=726177207061737465'
send-keys keys="saved=$(stty -g); stty -echo -icanon min 1 time 0; printf '\\033[?2004hWRAPPED_%s\\n' READY; hex=$(dd bs=1 count=25 2>/dev/null | od -An -tx1 | tr -d ' \\n'); stty \"$saved\"; printf '\\033[?2004lWRAPPED_HEX=%s\\n' \"$hex\"\r"
wait-for pattern='WRAPPED_READY'
paste-attach text='wrapped paste'
wait-for pattern='WRAPPED_HEX=1b5b3230307e777261707065642070617374651b5b3230317e'
assert-screen contains='WRAPPED_HEX=1b5b3230307e777261707065642070617374651b5b3230317e'
