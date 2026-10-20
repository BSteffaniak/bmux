@viewport cols=80 rows=24
@shell sh
new-session
send-keys keys='exit\r'
sleep ms=500
send-keys keys="printf 'AFTER_%s\\n' EXIT\r"
# Admission can race asynchronous terminal-session retirement. Even if the
# send is admitted, the exited shell must never execute it successfully.
# The expected output is absent from the command text (PTY echo is not proof).
wait-for pattern='AFTER_EXIT' timeout=3000
