@driver real-attach
@timeout 60000
@viewport cols=120 rows=36
@shell sh
@sandbox-config packages/cli/tests/fixtures/playbooks/theme_picker_real_attach.toml
new-session name='bootstrap'
send-keys keys='printf "\\033[2J\\033[HMARK\\n"\r'
wait-for pattern='MARK'
# The pick-theme command returns once the picker is open; preview, cancel and
# confirm all run afterwards over the command's captured async route.
send-attach key='alt+t'
wait-rendered contains='Use configured theme (active)' timeout=10000
# Moving to cyberpunk begins a live preview; Esc restores the previous theme,
# and only a successful restoration reports it.
send-attach key='down'
send-attach key='esc'
wait-rendered contains='theme: configured (preview restored)' timeout=10000
send-attach key='alt+t'
wait-rendered contains='Use configured theme (active)' timeout=10000
send-attach key='down'
send-attach key='enter'
# Reported only after the confirmation committed.
wait-rendered contains='theme: cyberpunk' timeout=10000
send-attach key='alt+t'
wait-rendered contains='cyberpunk (active)' timeout=10000
send-attach key='esc'
