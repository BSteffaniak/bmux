@name Jev plays Flappy Bird
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
# Inherit the explicitly selected demo application path and backend credential.
# Never put keys in @env entries: scripts and reports are executable evidence.
@env-mode inherit
@env PS1='$ '
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.tab_bar
new-session name='jev-flappy'
send-attach key='ctrl+b'
send-attach key='escape'
paste-attach text='exec python3 "$JEV_FLAPPY_APP"'
send-attach key='enter'
wait-for pattern='JEV FLAPPY BIRD' timeout=15000
# Script completion is intentional. The managed runner retains the actual
# sandbox until presenter Stop/on_leave; no sleep loop or timeout workaround.
