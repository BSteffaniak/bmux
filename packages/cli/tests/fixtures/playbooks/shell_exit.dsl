@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='exit\r'
sleep ms=500
send-keys keys='echo after_exit\r'
