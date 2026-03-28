@viewport cols=120 rows=40
@shell sh
new-session
split-pane direction=vertical
send-keys keys='echo pane1_marker\r' pane=1
sleep ms=500
assert-screen contains='pane1_marker' pane=1
send-keys keys='echo pane2_marker\r' pane=2
sleep ms=500
assert-screen contains='pane2_marker' pane=2
