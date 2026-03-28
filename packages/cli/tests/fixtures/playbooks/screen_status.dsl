@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo visible_text_marker\r'
sleep ms=500
screen
status
