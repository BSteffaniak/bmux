@name Jev plays Flappy Bird
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
# Declare public executable inputs separately from private host environment values.
@resource FLAPPY_APP=jev_flappy.py
@env-from JEV_API_KEY JEV_MODEL
@env-mode clean
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
paste-attach text='exec python3 "$FLAPPY_APP" --control-directory "$BMUX_RUNTIME_DIR"'
send-attach key='enter'
wait-for pattern='JEV FLAPPY BIRD' timeout=15000
# Script completion is intentional. The managed runner retains the actual
# sandbox until presenter Stop/on_leave; no sleep loop or timeout workaround.
