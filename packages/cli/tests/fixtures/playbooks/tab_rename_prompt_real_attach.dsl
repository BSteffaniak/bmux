@driver real-attach
@timeout 300000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/finder_relevance.toml
new-session name='bootstrap'
send-attach key='alt+a'
wait-rendered contains='<active:deps>' timeout=60000
# The prompt outlives the rename-tab command; the rename must still apply.
send-attach key='alt+r'
wait-rendered contains='Rename tab' timeout=60000
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='backspace'
send-attach key='n'
send-attach key='e'
send-attach key='w'
send-attach key='space'
send-attach key='n'
send-attach key='m'
send-attach key='enter'
wait-rendered contains='<active:new nm>' timeout=60000
