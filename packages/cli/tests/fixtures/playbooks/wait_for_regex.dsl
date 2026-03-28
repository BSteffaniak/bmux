@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='echo item_42_done\r'
wait-for pattern='item_\d+_done'
