@name Real output and scrollback
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 45000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.tab_bar
new-session name='rendering'
send-attach key='ctrl+b'
# Number real package manifests; cap at 100 so the final row is a stable landmark.
send-attach key='escape'
paste-attach text='find packages plugins -name Cargo.toml | sort | head -n 100 | nl -ba'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^\s*100\s+.*Cargo.toml'
assert-screen matches='(?m)^\s*100\s+.*Cargo.toml'
sleep ms=1800
send-attach key='['
sleep ms=1200
send-attach key='page_up'
sleep ms=1800
send-attach key='page_down'
sleep ms=1800
send-attach key='escape'
sleep ms=700
# Leaving scrollback restores the live terminal at its last manifest.
assert-screen matches='(?m)^\s*100\s+.*Cargo.toml'
snapshot id='catalog-scrollback-restored'
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1800
