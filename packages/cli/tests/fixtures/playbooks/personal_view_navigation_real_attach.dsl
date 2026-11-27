@driver real-attach
@timeout 300000
@viewport cols=120 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/personal_view_navigation_real_attach.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:alpha>' timeout=60000
send-attach key='alt+b'
wait-rendered contains='<active:beta>' timeout=60000
send-keys keys='printf "\\033[2J\\033[HBETA_READY\\n"\r'
wait-for pattern='BETA_READY' timeout=60000
send-attach key='alt+c'
wait-rendered contains='<active:gamma>' timeout=60000
send-keys keys='printf "\\033[2J\\033[HGAMMA_READY\\n"\r'
wait-for pattern='GAMMA_READY' timeout=60000
# Remove beta from this attachment's view: the strip drops it and the
# attachment moves to the next member, gamma. Beta keeps running.
send-attach key='alt+h'
wait-rendered contains='<active:beta>' timeout=60000
send-attach key='alt+x'
wait-rendered contains='<active:gamma>' timeout=60000
assert-rendered matches='│ +<[a-z]+:alpha>[^<]*<active:gamma> '
# Keyboard navigation walks the visible order: gamma's neighbours are alpha
# on both sides of the removed tab.
send-attach key='alt+h'
wait-rendered contains='<active:alpha>' timeout=60000
send-attach key='alt+j'
wait-rendered contains='<active:gamma>' timeout=60000
send-attach key='alt+1'
wait-rendered contains='<active:alpha>' timeout=60000
send-attach key='alt+2'
wait-rendered contains='<active:gamma>' timeout=60000
# The finder lists every tab and marks the removed one; opening it adds it at
# the end of the view and switches to it.
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=60000
send-attach key='b'
send-attach key='e'
send-attach key='t'
send-attach key='a'
wait-rendered contains='not in your view' timeout=60000
send-attach key='enter'
wait-rendered contains='<active:beta>' timeout=60000
wait-for pattern='BETA_READY' timeout=60000
assert-rendered matches='│ +<[a-z]+:alpha>[^<]*<[a-z]+:gamma>[^<]*<active:beta> '
# Keyboard and goto agree with the new visible order.
send-attach key='alt+h'
wait-rendered contains='<active:gamma>' timeout=60000
send-attach key='alt+3'
wait-rendered contains='<active:beta>' timeout=60000
send-attach key='alt+2'
wait-rendered contains='<active:gamma>' timeout=60000
assert-screen contains='GAMMA_READY'
