@name A demonstration with checks
@viewport cols=80 rows=24
@shell sh
@timeout 30000
new-session name='evidence'
send-keys keys="printf '\\033[2J\\033[H'; printf 'Arrange -> act -> observe -> assert\\n'\r"
wait-for pattern='Arrange -> act -> observe -> assert'
sleep ms=1000
send-keys keys="printf 'The command actually ran in an isolated shell.\\n'\r"
wait-for pattern='The command actually ran in an isolated shell.'
assert-layout pane_count=1
assert-screen contains='Arrange -> act -> observe -> assert'
snapshot id='verified-demo'
sleep ms=2000
send-keys keys="printf 'PASS / checked output, not a prerecorded animation\\n'\r"
wait-for pattern='PASS / checked output'
sleep ms=1500
