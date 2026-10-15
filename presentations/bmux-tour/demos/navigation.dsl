@name Return to the same running shell
@viewport cols=100 rows=30
@shell sh
@input-profile tmux_compat
@env PS1='$ '
@timeout 60000
@plugin enable=bmux.sessions
@plugin enable=bmux.contexts
@plugin enable=bmux.clients
@plugin enable=bmux.tabs
@plugin enable=bmux.workspaces
@plugin enable=bmux.tab_bar
new-session name='api'
send-attach key='ctrl+b'
# Save shell identity and a private job-list fixture before changing views.
send-attach key='escape'
paste-attach text='tour_origin=$$; seq 1 42 > "$BMUX_RUNTIME_DIR/navigation-jobs"; pwd'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
sleep ms=1800
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='date -u'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='UTC'
sleep ms=1800
send-attach key='ctrl+h'
sleep ms=700
send-attach key='escape'
# Only the original shell can satisfy this identity check and count its fixture.
paste-attach text='test "$tour_origin" = "$$" && wc -l < "$BMUX_RUNTIME_DIR/navigation-jobs"'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^42\s*$'
assert-screen matches='(?m)^42\s*$'
snapshot id='original-shell-restored'
sleep ms=1800
send-attach key='ctrl+s'
sleep ms=1600
# Create a real workspace using its default shortcut and generated name.
send-attach key='shift+w'
sleep ms=1000
send-attach key='c'
sleep ms=700
send-attach key='escape'
paste-attach text='uname -sr'
send-attach key='enter'
sleep ms=500
send-attach key='ctrl+b'
wait-for pattern='(?m)^Linux '
sleep ms=1800
send-attach key='alt+l'
sleep ms=700
assert-screen contains='UTC'
sleep ms=1800
