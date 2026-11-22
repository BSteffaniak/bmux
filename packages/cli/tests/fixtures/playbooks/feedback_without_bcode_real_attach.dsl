@driver real-attach
@timeout 60000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/feedback_without_bcode.toml
@env BCODE_BIN=
new-session name='feedback'
# Feedback given without a usable Bcode is reported, not lost silently.
send-attach key='alt+g'
wait-rendered contains='Feedback for Bcode' timeout=10000
send-attach key='h'
send-attach key='i'
send-attach key='enter'
wait-rendered contains='Feedback not sent' timeout=20000
