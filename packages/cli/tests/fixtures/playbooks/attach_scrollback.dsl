@timeout 20000
@shell sh

new-session
send-keys keys='seq 1 200\r'
wait-for pattern='200'

# Enter scrollback mode and move the cursor up one line via attach key handling.
send-attach key='ctrl+a ['
send-attach key='k'
send-attach key='enter'

screen
assert-screen not_contains='[k'
assert-screen not_contains='not found'
