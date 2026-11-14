@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/workspaces_real_attach.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:tab-A_ONE>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HA_ONE\\n"\r'
wait-for pattern='A_ONE'
send-attach key='alt+b'
wait-rendered contains='<active:tab-A_TWO>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HA_TWO\\n"\r'
wait-for pattern='A_TWO'
# A new workspace creates and selects an empty tab-1 context, not a name prompt.
send-attach key='alt+w'
wait-rendered contains='<active:tab-1>' timeout=10000
send-attach key='alt+d'
wait-rendered contains='<active:tab-B_ONE>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HB_ONE\\n"\r'
wait-for pattern='B_ONE'
send-attach key='alt+e'
wait-rendered contains='<active:tab-B_TWO>' timeout=10000
send-keys keys='printf "\\033[2J\\033[HB_TWO\\n"\r'
wait-for pattern='B_TWO'
send-attach key='alt+l'
wait-rendered contains='A_TWO' timeout=10000
wait-for pattern='A_TWO' timeout=10000
assert-screen contains='A_TWO'
# MRU excluding current: B_TWO, B_ONE, A_ONE; not workspace order.
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=10000
send-attach key='enter'
wait-rendered contains='B_TWO' timeout=10000
wait-for pattern='B_TWO' timeout=10000
assert-screen contains='B_TWO'
# After that visit: A_TWO, B_ONE, A_ONE. Filtering must retain recency.
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=10000
send-attach key='t'
send-attach key='a'
send-attach key='b'
wait-rendered contains='tab' timeout=10000
send-attach key='down'
send-attach key='enter'
wait-rendered contains='B_ONE' timeout=10000
wait-for pattern='B_ONE' timeout=10000
assert-screen contains='B_ONE'
