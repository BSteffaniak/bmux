@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/finder_relevance.toml
new-session name='bootstrap'
send-attach key='alt+a'
# New tabs may have no chrome in this fixture; pane output below is readiness.
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HDEPS_READY\\n"\r'
wait-for pattern='DEPS_READY' timeout=10000
send-attach key='alt+r'
wait-rendered contains='Rename tab' timeout=10000
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='d'
send-attach key='e'
send-attach key='p'
send-attach key='s'
send-attach key='enter'
wait-rendered contains='DEPS_READY' timeout=10000
send-attach key='alt+b'
# New tabs may have no chrome in this fixture; pane output below is readiness.
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HLOOSE_READY\\n"\r'
wait-for pattern='LOOSE_READY' timeout=10000
send-attach key='alt+r'
wait-rendered contains='Rename tab' timeout=10000
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='d'
send-attach key='-'
send-attach key='e'
send-attach key='-'
send-attach key='p'
send-attach key='-'
send-attach key='s'
send-attach key='enter'
wait-rendered contains='LOOSE_READY' timeout=10000
send-attach key='alt+c'
# New tabs may have no chrome in this fixture; pane output below is readiness.
sleep ms=400
send-keys keys='printf "\\033[2J\\033[HCURRENT_READY\\n"\r'
wait-for pattern='CURRENT_READY' timeout=10000
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=10000
send-attach key='d'
send-attach key='e'
send-attach key='p'
send-attach key='s'
wait-rendered contains='default/deps' timeout=10000
wait-rendered contains='default/d-e-p-s' timeout=10000
assert-rendered contains='default/deps'
assert-rendered contains='default/d-e-p-s'
send-attach key='enter'
wait-rendered contains='DEPS_READY' timeout=10000
wait-for pattern='DEPS_READY' timeout=10000
# Opening again hides the selected exact match, leaving only the loose match
# for the same query. This checks actual selection without pane-index assumptions.
send-attach key='alt+f'
wait-rendered contains='Find Tab' timeout=10000
send-attach key='d'
send-attach key='e'
send-attach key='p'
send-attach key='s'
wait-rendered contains='1/1 matches' timeout=10000
wait-rendered contains='default/d-e-p-s' timeout=10000
assert-rendered contains='1/1 matches'
assert-rendered contains='default/d-e-p-s'
send-attach key='esc'
