@driver real-attach
@timeout 300000
@viewport cols=100 rows=30
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/pane_padding_configure_real_attach.toml
new-session name='bootstrap'
send-keys keys='printf "\\033[2J\\033[HMARK\\n"\r'
wait-for pattern='MARK' timeout=60000
wait-rendered contains='│MARK' timeout=60000
# The configure command returns once the form is open; live preview, cancel
# and commit all happen afterwards, without the command's host route.
send-attach key='alt+p'
wait-rendered contains='Pane Padding' timeout=60000
# Seven fields down is the Left edge; shift+right adds five cells.
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='shift+right'
wait-rendered contains='│     MARK' timeout=60000
# Cancel restores the previous padding.
send-attach key='esc'
wait-rendered contains='│MARK' timeout=60000
send-attach key='alt+p'
wait-rendered contains='Pane Padding' timeout=60000
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='down'
send-attach key='shift+right'
send-attach key='shift+right'
wait-rendered contains='│          MARK' timeout=60000
# A second session in the same attach process; submit commits, then the
# configurator reports the applied padding.
send-attach key='enter'
wait-rendered contains='pane padding: edges 10/0/0/0' timeout=60000
assert-rendered contains='│          MARK'
