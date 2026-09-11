@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='bootstrap'
send-attach key='c'
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HA_ONE\\n"\r'
wait-for pattern='A_ONE'
send-attach key='c'
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HA_TWO\\n"\r'
wait-for pattern='A_TWO'
send-attach key='alt+w'
sleep ms=300
send-attach key='b'
send-attach key='enter'
sleep ms=500
send-keys keys='printf "\\033[2J\\033[HB_ONE\\n"\r'
wait-for pattern='B_ONE'
send-attach key='c'
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HB_TWO\\n"\r'
wait-for pattern='B_TWO'
send-attach key='alt+l'
sleep ms=400
assert-screen contains='A_TWO'
# MRU excluding current: B_TWO, B_ONE, A_ONE; not workspace order.
send-attach key='alt+f'
sleep ms=300
send-attach key='enter'
sleep ms=400
assert-screen contains='B_TWO'
# After that visit: A_TWO, B_ONE, A_ONE. Filtering must retain recency.
send-attach key='alt+f'
sleep ms=300
send-attach key='t'
send-attach key='a'
send-attach key='b'
send-attach key='down'
send-attach key='enter'
sleep ms=400
assert-screen contains='B_ONE'
